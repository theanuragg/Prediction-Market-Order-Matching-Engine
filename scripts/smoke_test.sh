#!/usr/bin/env bash
# End-to-end smoke test. Requires the full stack running:
#   docker compose up --build -d
# Then: ./scripts/smoke_test.sh

set -euo pipefail

API1="http://localhost:8080"
API2="http://localhost:8081"
PASS=0
FAIL=0

green() { echo -e "\033[32m✓ $1\033[0m"; PASS=$((PASS+1)); }
red()   { echo -e "\033[31m✗ $1\033[0m"; FAIL=$((FAIL+1)); }
info()  { echo -e "\033[34m→ $1\033[0m"; }

assert_eq() {
    local desc="$1" got="$2" want="$3"
    if [ "$got" = "$want" ]; then green "$desc"; else red "$desc (got '$got', want '$want')"; fi
}

wait_for_endpoint() {
    local name="$1" url="$2"
    for _ in $(seq 1 30); do
        if curl -sf "$url" >/dev/null 2>&1; then
            green "$name is ready"
            return 0
        fi
        sleep 1
    done
    red "$name not responding at $url"
    exit 1
}

reset_state() {
    info "Resetting stack state..."
    docker compose stop api1 api2 matcher >/dev/null
    docker compose exec -T redis redis-cli FLUSHALL >/dev/null
    docker compose up -d matcher api1 api2 >/dev/null
    wait_for_endpoint "API1" "$API1/orderbook"
    wait_for_endpoint "API2" "$API2/orderbook"
}

info "Waiting for API servers..."
wait_for_endpoint "API1" "$API1/orderbook"
wait_for_endpoint "API2" "$API2/orderbook"

#        Reset state                                                                                                                                                                                              
reset_state

#        Test 1: Submit order, get ID back                                                                                                                         
info "Test 1: POST /orders returns an ID"
R=$(curl -sf -X POST "$API1/orders" \
    -H 'Content-Type: application/json' \
    -d '{"side":"sell","price":100,"qty":10}')
ID1=$(echo "$R" | python3 -c "import sys,json; print(json.load(sys.stdin)['order_id'])")
assert_eq "order_id is a number" "$(echo "$ID1" | grep -E '^[0-9]+$' | wc -l | tr -d ' ')" "1"

#        Test 2: Orderbook shows the resting sell                                                                                                       
info "Test 2: GET /orderbook shows resting sell"
sleep 0.3
ASKS=$(curl -sf "$API1/orderbook" | python3 -c "import sys,json; b=json.load(sys.stdin); print(b['asks'][0]['price'],b['asks'][0]['qty'])")
assert_eq "ask @ 100 qty 10" "$ASKS" "100 10"

#        Test 3: Cross-instance match                                                                                                                                        
info "Test 3: BUY submitted to instance 2 matches SELL from instance 1"
R=$(curl -sf -X POST "$API2/orders" \
    -H 'Content-Type: application/json' \
    -d '{"side":"buy","price":100,"qty":6}')
ID2=$(echo "$R" | python3 -c "import sys,json; print(json.load(sys.stdin)['order_id'])")
assert_eq "second order_id incremented" "$ID2" "2"

sleep 0.3
BOOK=$(curl -sf "$API1/orderbook")
BIDS=$(echo "$BOOK" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['bids']))")
ASKS_QTY=$(echo "$BOOK" | python3 -c "import sys,json; b=json.load(sys.stdin); print(b['asks'][0]['qty'] if b['asks'] else 0)")
assert_eq "bids empty after partial fill" "$BIDS" "0"
assert_eq "ask has 4 remaining after partial fill" "$ASKS_QTY" "4"

#        Test 4: Same book from instance 2                                                                                                                         
info "Test 4: Both instances return identical orderbook"
BOOK2=$(curl -sf "$API2/orderbook")
ASKS_QTY2=$(echo "$BOOK2" | python3 -c "import sys,json; b=json.load(sys.stdin); print(b['asks'][0]['qty'] if b['asks'] else 0)")
assert_eq "instance 2 also shows qty 4" "$ASKS_QTY2" "4"

#        Test 5: Complete the fill                                                                                                                                                    
info "Test 5: Second buy fully clears the book"
curl -sf -X POST "$API1/orders" \
    -H 'Content-Type: application/json' \
    -d '{"side":"buy","price":100,"qty":4}' >/dev/null
sleep 0.3
FINAL=$(curl -sf "$API1/orderbook")
FINAL_BIDS=$(echo "$FINAL" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['bids']))")
FINAL_ASKS=$(echo "$FINAL" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['asks']))")
assert_eq "book fully empty - bids" "$FINAL_BIDS" "0"
assert_eq "book fully empty - asks" "$FINAL_ASKS" "0"

#        Test 6: Validation                                                                                                                                                                         
info "Test 6: price=0 returns 422"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$API1/orders" \
    -H 'Content-Type: application/json' -d '{"side":"buy","price":0,"qty":5}')
assert_eq "price=0 → 422" "$STATUS" "422"

info "Test 7: qty=0 returns 422"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$API1/orders" \
    -H 'Content-Type: application/json' -d '{"side":"buy","price":10,"qty":0}')
assert_eq "qty=0 → 422" "$STATUS" "422"

#        Test 8: Price-time priority                                                                                                                                              
info "Test 8: Price-time priority — lower ask matches first"
reset_state
curl -sf -X POST "$API1/orders" -H 'Content-Type: application/json' -d '{"side":"sell","price":102,"qty":5}' >/dev/null
curl -sf -X POST "$API1/orders" -H 'Content-Type: application/json' -d '{"side":"sell","price":100,"qty":5}' >/dev/null
curl -sf -X POST "$API1/orders" -H 'Content-Type: application/json' -d '{"side":"sell","price":101,"qty":5}' >/dev/null
sleep 0.3
# Buy 7 @ 103 — should eat @ 100 (5) then @ 101 (2), leaving @ 101 (3) and @ 102 (5)
curl -sf -X POST "$API2/orders" -H 'Content-Type: application/json' -d '{"side":"buy","price":103,"qty":7}' >/dev/null
sleep 0.3
BOOK=$(curl -sf "$API1/orderbook")
BEST_ASK=$(echo "$BOOK" | python3 -c "import sys,json; b=json.load(sys.stdin); print(b['asks'][0]['price'] if b['asks'] else 'none')")
assert_eq "best remaining ask is 101 (price priority correct)" "$BEST_ASK" "101"

#        Summary                                                                                                                                                                                                          
echo ""
echo "                                                                                                "
echo "  PASSED: $PASS"
if [ "$FAIL" -gt 0 ]; then
    echo "  FAILED: $FAIL"
    exit 1
else
    echo "  All tests passed ✓"
fi
