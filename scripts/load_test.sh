
set -e

API1="http://localhost:8080"
API2="http://localhost:8081"

# Colors
RED='\033[31m'
GREEN='\033[32m'
BLUE='\033[34m'
CYAN='\033[36m'
RESET='\033[0m'

# Logging functions
info()  { echo -e "${BLUE}→${RESET} $1"; }
stat()  { echo -e "${CYAN}◆${RESET} $1"; }
green() { echo -e "${GREEN}✓${RESET} $1"; }
red()   { echo -e "${RED}✗${RESET} $1"; }
header() { echo -e "\n${CYAN}════════════════════════════════════════════════════════════════${RESET}"; echo -e "${CYAN}  $1${RESET}"; echo -e "${CYAN}════════════════════════════════════════════════════════════════${RESET}\n"; }

# Wait for server to be ready
wait_for_server() {
    info "Waiting for API servers..."
    for i in {1..20}; do
        if curl -sf "$API1/orderbook" >/dev/null 2>&1; then
            green "Server is ready"
            return 0
        fi
        sleep 1
    done
    red "ERROR: Server not responding"
    exit 1
}

# Reset Redis state
reset_redis() {
    info "Resetting Redis state..."
    redis-cli -p 6379 FLUSHALL >/dev/null 2>&1 || true
    sleep 0.5
}

# Core load test function
run_load_test() {
    local rate=$1
    local duration=$2
    local name=$3
    
    header "$name: $rate orders/sec for $duration seconds"
    
    orders_sent=0
    errors=0
    start_time=$(date +%s)
    interval=$(echo "scale=4; 1 / $rate" | bc)
    elapsed=0
    
    info "Submitting orders..."
    
    while [ $elapsed -lt $duration ]; do
        api=$([[ $((orders_sent % 2)) -eq 0 ]] && echo "$API1" || echo "$API2")
        side=$([[ $((orders_sent % 3)) -eq 0 ]] && echo "buy" || echo "sell")
        price=$((95 + RANDOM % 11))
        qty=$((1 + RANDOM % 10))
        
        response=$(curl -sf -X POST "$api/orders" \
            -H 'Content-Type: application/json' \
            -d "{\"side\":\"$side\",\"price\":$price,\"qty\":$qty}" 2>/dev/null || echo '{"error":"failed"}')
        
        if echo "$response" | grep -q "order_id"; then
            orders_sent=$((orders_sent + 1))
            if [ $((orders_sent % 100)) -eq 0 ]; then
                stat "Orders sent: $orders_sent"
            fi
        else
            errors=$((errors + 1))
        fi
        
        sleep "$interval"
        elapsed=$(($(date +%s) - start_time))
    done
    
    sleep 1
    
    # Get final orderbook state
    info "Checking final orderbook state..."
    orderbook=$(curl -sf "$API1/orderbook")
    bids=$(echo "$orderbook" | python3 -c "import sys,json; b=json.load(sys.stdin); print(len(b.get('bids',[])))" 2>/dev/null || echo "?")
    asks=$(echo "$orderbook" | python3 -c "import sys,json; b=json.load(sys.stdin); print(len(b.get('asks',[])))" 2>/dev/null || echo "?")
    
    # Results
    elapsed=$(($(date +%s) - start_time))
    throughput=$(echo "scale=1; $orders_sent / $elapsed" | bc)
    
    echo ""
    stat "Test complete in $elapsed seconds"
    stat "Orders sent: $orders_sent"
    stat "Errors: $errors"
    stat "Throughput: $throughput orders/sec"
    stat "Final orderbook levels: $bids bids, $asks asks"
}

# Test 1: Baseline
test_baseline() {
    reset_redis
    run_load_test 100 20 "TEST 1: BASELINE THROUGHPUT"
}

# Test 2: Sustained load
test_sustained() {
    reset_redis
    run_load_test 500 30 "TEST 2: SUSTAINED HIGH LOAD"
}

# Test 3: Traffic spike
test_spike() {
    header "TEST 3: TRAFFIC SPIKE PATTERN"
    
    info "Phase 1: Light load (50 orders/sec, 10s)"
    reset_redis
    run_load_test 50 10 "Phase 1"
    
    info "Phase 2: Sudden spike (1000 orders/sec, 10s)"
    run_load_test 1000 10 "Phase 2"
    
    info "Phase 3: Back to normal (50 orders/sec, 10s)"
    run_load_test 50 10 "Phase 3"
}

# Test 4: Deep order book
test_deep_book() {
    header "TEST 4: DEEP ORDER BOOK (many price levels)"
    reset_redis
    
    info "Building deep book with 40 price levels..."
    for price in {80..120}; do
        qty=$((1 + RANDOM % 10))
        side=$([[ $((price % 2)) -eq 0 ]] && echo "buy" || echo "sell")
        
        curl -sf -X POST "$API1/orders" \
            -H 'Content-Type: application/json' \
            -d "{\"side\":\"$side\",\"price\":$price,\"qty\":$qty}" >/dev/null 2>&1 &
        
        if [ $((price % 10)) -eq 0 ]; then
            stat "Added orders at prices up to: $price"
        fi
    done
    wait
    sleep 1
    
    info "Testing with high load on deep book..."
    run_load_test 200 20 "Deep Book Test"
}

# Test 5: Large orders
test_large_orders() {
    header "TEST 5: LARGE ORDERS (multi-level sweeps)"
    reset_redis
    
    info "Building resting sell book (20 levels × 100 qty)..."
    for i in {1..20}; do
        price=$((95 + i))
        curl -sf -X POST "$API1/orders" \
            -H 'Content-Type: application/json' \
            -d "{\"side\":\"sell\",\"price\":$price,\"qty\":100}" >/dev/null 2>&1 &
    done
    wait
    sleep 1
    
    info "Submitting large buy orders to sweep multiple levels..."
    for i in {1..5}; do
        response=$(curl -sf -X POST "$API1/orders" \
            -H 'Content-Type: application/json' \
            -d "{\"side\":\"buy\",\"price\":120,\"qty\":$((200 + i * 50))}")
        order_id=$(echo "$response" | python3 -c "import sys,json; print(json.load(sys.stdin)['order_id'])" 2>/dev/null || echo "?")
        stat "Large buy order $i: $((200 + i * 50)) qty (ID: $order_id)"
        sleep 0.3
    done
    
    sleep 2
    green "Large orders test complete"
}

# Test 6: Cross-instance consistency
test_cross_instance() {
    header "TEST 6: CROSS-INSTANCE CONSISTENCY"
    reset_redis
    
    info "Submitting orders to both instances..."
    for i in {1..10}; do
        side=$([[ $((i % 2)) -eq 0 ]] && echo "buy" || echo "sell")
        price=$((95 + RANDOM % 11))
        curl -sf -X POST "$API1/orders" \
            -H 'Content-Type: application/json' \
            -d "{\"side\":\"$side\",\"price\":$price,\"qty\":5}" >/dev/null 2>&1 &
    done
    wait
    sleep 1
    
    book1=$(curl -sf "$API1/orderbook")
    book2=$(curl -sf "$API2/orderbook")
    
    if [ "$book1" = "$book2" ]; then
        green "✓ Orderbooks match across instances"
    else
        red "WARNING: Orderbooks differ"
    fi
    
    info "Testing cross-instance matching (sell on API1, buy on API2)..."
    
    sell=$(curl -sf -X POST "$API1/orders" \
        -H 'Content-Type: application/json' \
        -d '{"side":"sell","price":100,"qty":10}')
    sell_id=$(echo "$sell" | python3 -c "import sys,json; print(json.load(sys.stdin)['order_id'])" 2>/dev/null || echo "?")
    stat "Sell order $sell_id @ 100 qty 10 on API1"
    
    sleep 0.5
    
    buy=$(curl -sf -X POST "$API2/orders" \
        -H 'Content-Type: application/json' \
        -d '{"side":"buy","price":100,"qty":10}')
    buy_id=$(echo "$buy" | python3 -c "import sys,json; print(json.load(sys.stdin)['order_id'])" 2>/dev/null || echo "?")
    stat "Buy order $buy_id @ 100 qty 10 on API2"
    
    sleep 1
    
    final=$(curl -sf "$API1/orderbook")
    book_size=$(echo "$final" | python3 -c "import sys,json; b=json.load(sys.stdin); print(len(b['bids']) + len(b['asks']))" 2>/dev/null || echo "unknown")
    
    if [ "$book_size" = "0" ]; then
        green "✓ Cross-instance matching successful (book empty after complete fill)"
    else
        red "WARNING: Book not empty after matching (size: $book_size)"
    fi
}

# Main menu
show_help() {
    cat << 'EOF'
╔═══════════════════════════════════════════════════════════════════╗
║          PREDICTION MARKET LOAD TEST - All Scenarios in One       ║
╚═══════════════════════════════════════════════════════════════════╝

USAGE:
  ./load_test.sh [scenario] [rate] [duration]

SCENARIOS:
  baseline        - 100 orders/sec for 20s (default)
  sustained       - 500 orders/sec for 30s (sustained load)
  spike           - 3-phase spike test (50→1000→50 orders/sec)
  deep_book       - Deep order book with 40 price levels
  large_orders    - Large orders sweeping multiple price levels
  cross_instance  - Multi-instance consistency validation
  all             - Run all scenarios (~5-10 minutes)
  custom RATE DUR - Custom: RATE orders/sec for DUR seconds

EXAMPLES:
  ./load_test.sh                        # Run baseline (100 orders/sec)
  ./load_test.sh spike                  # Run spike test
  ./load_test.sh custom 1000 60         # 1000 orders/sec for 60 seconds
  ./load_test.sh all                    # Run all scenarios

REQUIREMENTS:
  - Docker Compose stack running (docker compose up -d)
  - redis-cli installed
  - Python 3 (for JSON parsing)
  - curl (usually pre-installed)

MONITORING:
  In another terminal, watch the matcher:
    docker logs -f prediction-market-matcher-1

  Or watch system resources:
    docker stats prediction-market-matcher-1

EOF
}

# Main logic
main() {
    scenario="${1:-baseline}"
    rate="${2:-}"
    duration="${3:-}"
    
    wait_for_server
    
    case "$scenario" in
        baseline)
            test_baseline
            ;;
        sustained)
            test_sustained
            ;;
        spike)
            test_spike
            ;;
        deep_book)
            test_deep_book
            ;;
        large_orders)
            test_large_orders
            ;;
        cross_instance)
            test_cross_instance
            ;;
        all)
            header "COMPREHENSIVE TEST SUITE - All Scenarios"
            test_baseline
            sleep 2
            test_deep_book
            sleep 2
            test_spike
            sleep 2
            test_cross_instance
            sleep 2
            test_sustained
            header "COMPREHENSIVE TEST SUITE COMPLETE ✓"
            ;;
        custom)
            if [ -z "$rate" ] || [ -z "$duration" ]; then
                red "Custom test requires rate and duration"
                echo "Usage: ./load_test.sh custom RATE DURATION"
                echo "Example: ./load_test.sh custom 1000 60"
                exit 1
            fi
            reset_redis
            run_load_test "$rate" "$duration" "CUSTOM TEST"
            ;;
        help|--help|-h)
            show_help
            ;;
        *)
            red "Unknown scenario: $scenario"
            echo ""
            show_help
            exit 1
            ;;
    esac
    
    header "LOAD TEST COMPLETE ✓"
}

# Run main
main "$@"
