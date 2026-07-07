//! Tool-call grammar for the orders integration. Companion to
//! `prompt.rs`: the system prompt teaches the LLM to emit lines like
//!
//! ```text
//! <TOOL>list_products</TOOL>
//! <TOOL>add_item product="Margherita" quantity=1 extras="Bacon,Extra cheese"</TOOL>
//! <TOOL>place_order customer_name="Lance" customer_phone="+61432..."</TOOL>
//! ```
//!
//! and this module turns those into typed `OrderToolCall` values, runs
//! them against the orders DB while accumulating a per-batch in-flight
//! cart (so a sequence of `add_item` calls in one model output flows
//! into one `place_order` write), and renders the results back as text
//! the model uses to phrase a natural reply.
//!
//! Reuses the kv-pair parser from `crate::calendar::tools::parse_kv_pairs`
//! via the same `<TOOL>` envelope — running the parser twice (once for
//! calendar, once for orders) is cheap and lets each module own its own
//! ToolCall enum without a shared union type. The bot integration calls
//! `extract_order_tool_calls` to pull just the orders ones out of an
//! LLM output.
//!
//! Money in/out of this module is integer cents — see `mod.rs`.

use rusqlite::Connection;

use super::{
    cancel_order, compute_line_total, get_order, list_products_with_extras, place_order,
    AppliedExtra, OrderError, OrderInput, OrderItemInput, OrdersConfig, Product,
};

// ============================================================================
// Tool call types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderToolCall {
    /// List the active product catalogue. The bot uses this when the
    /// customer asks "what do you have" or similar.
    ListProducts,
    /// Append an item to the in-flight order cart. Returns the line
    /// total + running cart total. Multiple `add_item` calls within
    /// one model pass build up the same cart.
    AddItem {
        product: String,
        quantity: u32,
        /// Comma-separated raw names from the LLM. Resolution against
        /// the product's catalogue happens in the executor.
        extras: Vec<String>,
        notes: Option<String>,
    },
    /// Compute the running total of the cart without flushing. Mostly
    /// useful when the customer asks "so what's the total" before
    /// confirming.
    QuoteOrder,
    /// Flush the cart to a real DB row. Customer info is captured here
    /// rather than on add_item so the bot can build the cart while
    /// still confirming name / phone with the customer.
    PlaceOrder {
        customer_name: Option<String>,
        customer_phone: Option<String>,
        notes: Option<String>,
    },
    /// Mark a previously placed order cancelled. Bot uses this when a
    /// customer changes their mind after `place_order` has already
    /// fired — without it, the model would have to ask the operator
    /// to do it manually.
    CancelOrder { order_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderToolParseError {
    pub raw: String,
    pub reason: String,
}

impl std::fmt::Display for OrderToolParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (raw: {})", self.reason, self.raw)
    }
}

/// Pull every `<TOOL>...</TOOL>` block out of `reply` and parse only
/// the ones whose head is an orders verb. Calendar verbs aren't in this
/// set so the calendar parser owns those — this lets the two modules
/// coexist in one LLM output without a shared dispatch table.
pub fn extract_order_tool_calls(reply: &str) -> Vec<Result<OrderToolCall, OrderToolParseError>> {
    let lower = reply.to_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < reply.len() {
        let Some(open_rel) = lower[cursor..].find("<tool>") else {
            break;
        };
        let open = cursor + open_rel + "<tool>".len();
        let Some(close_rel) = lower[open..].find("</tool>") else {
            break;
        };
        let close = open + close_rel;
        let body = reply[open..close].trim();
        if let Some(parsed) = try_parse_one(body) {
            out.push(parsed);
        }
        cursor = close + "</tool>".len();
    }
    out
}

fn try_parse_one(body: &str) -> Option<Result<OrderToolCall, OrderToolParseError>> {
    let raw = body.to_string();
    let (name, rest) = split_head(body);
    let lname = name.to_lowercase();
    if !is_order_verb(&lname) {
        return None; // belongs to a sibling module (calendar, etc.)
    }
    Some(parse_order_body(raw, &lname, rest))
}

fn parse_order_body(
    raw: String,
    lname: &str,
    rest: &str,
) -> Result<OrderToolCall, OrderToolParseError> {
    let kvs =
        crate::calendar::tools::parse_kv_pairs(rest).map_err(|reason| OrderToolParseError {
            raw: raw.clone(),
            reason,
        })?;
    match lname {
        "list_products" => Ok(OrderToolCall::ListProducts),
        "add_item" => {
            let product = kvs
                .get("product")
                .cloned()
                .ok_or_else(|| OrderToolParseError {
                    raw: raw.clone(),
                    reason: "add_item requires product=\"...\"".into(),
                })?;
            let quantity = kvs
                .get("quantity")
                .map(|s| s.parse::<u32>().unwrap_or(0))
                .unwrap_or(1);
            if quantity == 0 {
                return Err(OrderToolParseError {
                    raw,
                    reason: "add_item quantity must be >= 1".into(),
                });
            }
            let extras = kvs
                .get("extras")
                .map(|s| split_extras(s))
                .unwrap_or_default();
            Ok(OrderToolCall::AddItem {
                product,
                quantity,
                extras,
                notes: kvs.get("notes").cloned(),
            })
        }
        "quote_order" => Ok(OrderToolCall::QuoteOrder),
        "place_order" => Ok(OrderToolCall::PlaceOrder {
            customer_name: kvs.get("customer_name").cloned(),
            customer_phone: kvs.get("customer_phone").cloned(),
            notes: kvs.get("notes").cloned(),
        }),
        "cancel_order" => {
            let oid = kvs
                .get("order_id")
                .cloned()
                .ok_or_else(|| OrderToolParseError {
                    raw: raw.clone(),
                    reason: "cancel_order requires order_id=N".into(),
                })?;
            let parsed = oid.parse::<i64>().map_err(|e| OrderToolParseError {
                raw: raw.clone(),
                reason: format!("invalid order_id {:?}: {}", oid, e),
            })?;
            Ok(OrderToolCall::CancelOrder { order_id: parsed })
        }
        _ => unreachable!("filtered by is_order_verb"),
    }
}

/// True when `verb` (lower-case) is one of the orders module's tool
/// names. Exported so the calendar parser can skip them without
/// re-implementing the list — keeps the two sibling parsers from
/// stepping on each other when both walk the same `<TOOL>` envelope.
pub fn is_order_verb(verb: &str) -> bool {
    matches!(
        verb,
        "list_products" | "add_item" | "quote_order" | "place_order" | "cancel_order"
    )
}

fn split_head(body: &str) -> (&str, &str) {
    let trimmed = body.trim();
    match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
        None => (trimmed, ""),
    }
}

fn split_extras(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

// ============================================================================
// Tool execution
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderToolResult {
    Products {
        products: Vec<Product>,
    },
    /// One add_item succeeded — line total in cents and running cart
    /// grand total. The frontend never sees this directly; it's
    /// rendered via `render_results_for_llm` to feed back into pass-2.
    ItemAdded {
        product_name: String,
        quantity: u32,
        extras: Vec<AppliedExtra>,
        line_total_cents: i64,
        cart_total_cents: i64,
    },
    AddItemFailed {
        product: String,
        reason: String,
    },
    Quote {
        item_count: usize,
        cart_total_cents: i64,
    },
    Placed {
        order_id: i64,
        item_count: usize,
        total_cents: i64,
    },
    PlaceFailed {
        reason: String,
    },
    Cancelled {
        order_id: i64,
    },
    CancelFailed {
        order_id: i64,
        reason: String,
    },
    Error(String),
}

/// Drain a list of order tool calls and run them against the DB while
/// keeping a per-batch cart so `add_item ... add_item ... place_order`
/// in one model pass results in one persisted order. The cart is
/// dropped if `place_order` isn't called — that's intentional, the
/// bot's "I just want to add three items and ask the customer to
/// confirm before placing" flow needs to be able to spread across
/// turns without leaving stale state in the DB.
pub fn execute_order_tools(
    conn: &Connection,
    config: &OrdersConfig,
    calls: Vec<Result<OrderToolCall, OrderToolParseError>>,
    default_source: &str,
    trusted_phone: Option<&str>,
) -> Vec<OrderToolResult> {
    let mut out = Vec::new();
    if calls.is_empty() {
        return out;
    }
    let products = match list_products_with_extras(conn, true) {
        Ok(p) => p,
        Err(e) => {
            out.push(OrderToolResult::Error(format!(
                "could not load product catalogue: {}",
                e
            )));
            return out;
        }
    };
    let mut cart: Vec<OrderItemInput> = Vec::new();
    for call in calls {
        let res = match call {
            Err(e) => OrderToolResult::Error(e.to_string()),
            Ok(OrderToolCall::ListProducts) => OrderToolResult::Products {
                products: products.clone(),
            },
            Ok(OrderToolCall::AddItem {
                product,
                quantity,
                extras,
                notes,
            }) => match resolve_item(&products, &product, quantity, &extras, notes) {
                Ok(item) => {
                    let line_total = item.line_total_cents;
                    let pending = OrderItemInput {
                        product_id: item.product_id,
                        product_name: item.product_name.clone(),
                        base_price_cents: item.base_price_cents,
                        quantity: item.quantity,
                        extras: item.extras.clone(),
                        notes: item.notes,
                    };
                    cart.push(pending);
                    let cart_total: i64 = cart
                        .iter()
                        .map(|i| compute_line_total(i))
                        .fold(0i64, |a, b| a.saturating_add(b));
                    OrderToolResult::ItemAdded {
                        product_name: item.product_name,
                        quantity: item.quantity as u32,
                        extras: item.extras,
                        line_total_cents: line_total,
                        cart_total_cents: cart_total,
                    }
                }
                Err(reason) => OrderToolResult::AddItemFailed { product, reason },
            },
            Ok(OrderToolCall::QuoteOrder) => {
                let cart_total: i64 = cart
                    .iter()
                    .map(|i| compute_line_total(i))
                    .fold(0i64, |a, b| a.saturating_add(b));
                OrderToolResult::Quote {
                    item_count: cart.len(),
                    cart_total_cents: cart_total,
                }
            }
            Ok(OrderToolCall::PlaceOrder {
                customer_name,
                customer_phone,
                notes,
            }) => {
                if cart.is_empty() {
                    OrderToolResult::PlaceFailed {
                        reason: "no items added — call add_item first".into(),
                    }
                } else {
                    // Privacy: same gate as my_appointments. If the
                    // caller is on SMS / phone, ignore whatever phone
                    // the model echoed and stamp the trusted one.
                    let phone = trusted_phone
                        .map(|s| s.to_string())
                        .or(customer_phone.clone());
                    let to_place = OrderInput {
                        customer_phone: phone,
                        customer_name,
                        notes,
                        source: default_source.to_string(),
                        items: cart.clone(),
                    };
                    match place_order(conn, to_place) {
                        Ok(order_id) => {
                            let total: i64 = cart
                                .iter()
                                .map(|i| compute_line_total(i))
                                .fold(0i64, |a, b| a.saturating_add(b));
                            let count = cart.len();
                            cart.clear();
                            OrderToolResult::Placed {
                                order_id,
                                item_count: count,
                                total_cents: total,
                            }
                        }
                        Err(OrderError::Invalid(msg)) => {
                            OrderToolResult::PlaceFailed { reason: msg }
                        }
                        Err(OrderError::Sql(e)) => OrderToolResult::PlaceFailed {
                            reason: format!("database error: {}", e),
                        },
                    }
                }
            }
            Ok(OrderToolCall::CancelOrder { order_id }) => {
                // Look the order up first so we can (a) return a sensible
                // "not found" instead of a phantom-success when the model
                // hallucinates an id, and (b) gate cancel by phone when
                // the request came in over SMS / call. Without the gate,
                // anyone who guesses an id can SMS "cancel order #5" and
                // wipe a stranger's order.
                match get_order(conn, order_id) {
                    Ok(None) => OrderToolResult::CancelFailed {
                        order_id,
                        reason: format!("no order with id {}", order_id),
                    },
                    Ok(Some(existing)) => {
                        let owner_ok = match trusted_phone {
                            None => true,
                            Some(trusted) => {
                                let want = aokie_db::database::normalize_number(trusted);
                                existing
                                    .customer_phone
                                    .as_deref()
                                    .map(|p| aokie_db::database::normalize_number(p) == want)
                                    .unwrap_or(false)
                            }
                        };
                        if !owner_ok {
                            OrderToolResult::CancelFailed {
                                order_id,
                                reason: "that order isn't on this number".into(),
                            }
                        } else {
                            match cancel_order(conn, order_id) {
                                Ok(()) => OrderToolResult::Cancelled { order_id },
                                Err(e) => OrderToolResult::CancelFailed {
                                    order_id,
                                    reason: e.to_string(),
                                },
                            }
                        }
                    }
                    Err(e) => OrderToolResult::CancelFailed {
                        order_id,
                        reason: format!("database error: {}", e),
                    },
                }
            }
        };
        out.push(res);
    }
    // If the model emitted add_item calls but never place_order, drop
    // the cart silently — the next pass will rebuild from history.
    let _ = config;
    out
}

/// Look up a product by name, validate the quantity, and resolve the
/// requested extras against the product's own extras list. Extras the
/// catalogue doesn't recognise are surfaced as a parse error to the
/// model so it can ask the customer or pick something else, rather
/// than silently being added at zero cost.
fn resolve_item(
    products: &[Product],
    product_name: &str,
    quantity: u32,
    requested_extras: &[String],
    notes: Option<String>,
) -> Result<ResolvedItem, String> {
    let want = product_name.trim().to_lowercase();
    if want.is_empty() {
        return Err("product name is empty".into());
    }
    let product = products
        .iter()
        .find(|p| p.name.trim().to_lowercase() == want);
    let (product_id, product_name_canonical, base_price, available_extras) = match product {
        Some(p) => (
            Some(p.id),
            p.name.clone(),
            p.base_price_cents,
            p.extras.clone(),
        ),
        None => {
            // Unknown product. If the catalogue is empty (no products
            // configured yet) we still allow free-form to be polite,
            // but with a zero base price so the operator has to fix it.
            if products.is_empty() {
                (None, product_name.trim().to_string(), 0_i64, Vec::new())
            } else {
                return Err(format!(
                    "no product named '{}' — call list_products to see what's available",
                    product_name
                ));
            }
        }
    };
    let mut applied: Vec<AppliedExtra> = Vec::new();
    for raw in requested_extras {
        let want_extra = raw.trim().to_lowercase();
        if want_extra.is_empty() {
            continue;
        }
        let found = available_extras
            .iter()
            .find(|e| e.name.trim().to_lowercase() == want_extra);
        match found {
            Some(e) => applied.push(AppliedExtra {
                name: e.name.clone(),
                price_delta_cents: e.price_delta_cents,
            }),
            None => {
                return Err(format!(
                    "no extra called '{}' for {} — call list_products to see options",
                    raw, product_name_canonical
                ));
            }
        }
    }
    let extras_total: i64 = applied.iter().map(|e| e.price_delta_cents).sum();
    let unit = base_price.saturating_add(extras_total);
    let line_total = unit.saturating_mul(quantity as i64);
    Ok(ResolvedItem {
        product_id,
        product_name: product_name_canonical,
        base_price_cents: base_price,
        quantity: quantity as i64,
        extras: applied,
        notes,
        line_total_cents: line_total,
    })
}

#[derive(Debug, Clone)]
struct ResolvedItem {
    product_id: Option<i64>,
    product_name: String,
    base_price_cents: i64,
    quantity: i64,
    extras: Vec<AppliedExtra>,
    notes: Option<String>,
    line_total_cents: i64,
}

// ============================================================================
// Rendering for the LLM
// ============================================================================

pub fn render_results_for_llm(results: &[OrderToolResult], symbol: &str) -> String {
    let mut s = String::new();
    for (idx, r) in results.iter().enumerate() {
        if idx > 0 {
            s.push('\n');
        }
        match r {
            OrderToolResult::Products { products } => {
                if products.is_empty() {
                    s.push_str("[products] none configured");
                } else {
                    s.push_str("[products]\n");
                    for p in products {
                        s.push_str(&format!(
                            "- {} ({})",
                            p.name,
                            super::format_money(p.base_price_cents, symbol)
                        ));
                        if let Some(d) = p.description.as_deref() {
                            if !d.trim().is_empty() {
                                s.push_str(&format!(" — {}", d.trim()));
                            }
                        }
                        s.push('\n');
                        for e in &p.extras {
                            s.push_str(&format!(
                                "    + {} ({})\n",
                                e.name,
                                super::format_money(e.price_delta_cents, symbol)
                            ));
                        }
                    }
                }
            }
            OrderToolResult::ItemAdded {
                product_name,
                quantity,
                extras,
                line_total_cents,
                cart_total_cents,
            } => {
                let extras_str = if extras.is_empty() {
                    String::new()
                } else {
                    let names: Vec<&str> = extras.iter().map(|e| e.name.as_str()).collect();
                    format!(" + {}", names.join(", "))
                };
                s.push_str(&format!(
                    "[added] {}× {}{} = {} (cart total {})",
                    quantity,
                    product_name,
                    extras_str,
                    super::format_money(*line_total_cents, symbol),
                    super::format_money(*cart_total_cents, symbol)
                ));
            }
            OrderToolResult::AddItemFailed { product, reason } => {
                s.push_str(&format!(
                    "[add_item failed for {}: {}] — apologise and offer alternatives",
                    product, reason
                ));
            }
            OrderToolResult::Quote {
                item_count,
                cart_total_cents,
            } => {
                if *item_count == 0 {
                    s.push_str("[quote] cart is empty");
                } else {
                    s.push_str(&format!(
                        "[quote] {} item(s), running total {}",
                        item_count,
                        super::format_money(*cart_total_cents, symbol)
                    ));
                }
            }
            OrderToolResult::Placed {
                order_id,
                item_count,
                total_cents,
            } => {
                s.push_str(&format!(
                    "[placed order #{} — {} item(s), total {}]",
                    order_id,
                    item_count,
                    super::format_money(*total_cents, symbol)
                ));
            }
            OrderToolResult::PlaceFailed { reason } => {
                s.push_str(&format!(
                    "[place_order failed: {}] — apologise and ask if they want to retry",
                    reason
                ));
            }
            OrderToolResult::Cancelled { order_id } => {
                s.push_str(&format!("[cancelled order #{}]", order_id));
            }
            OrderToolResult::CancelFailed { order_id, reason } => {
                s.push_str(&format!(
                    "[cancel_order failed for #{}: {}] — apologise",
                    order_id, reason
                ));
            }
            OrderToolResult::Error(msg) => {
                s.push_str(&format!("[orders tool error] {}", msg));
            }
        }
    }
    s
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::{ProductExtraInput, ProductInput};

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE products (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                description TEXT,
                base_price_cents INTEGER NOT NULL CHECK(base_price_cents >= 0),
                active INTEGER NOT NULL DEFAULT 1,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE product_extras (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                price_delta_cents INTEGER NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                UNIQUE(product_id, name)
            );
            CREATE TABLE orders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                customer_phone TEXT,
                customer_name TEXT,
                notes TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                source TEXT NOT NULL DEFAULT 'manual',
                total_cents INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE order_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
                product_id INTEGER REFERENCES products(id) ON DELETE SET NULL,
                product_name TEXT NOT NULL,
                base_price_cents INTEGER NOT NULL,
                quantity INTEGER NOT NULL CHECK(quantity > 0),
                extras_json TEXT NOT NULL DEFAULT '[]',
                line_total_cents INTEGER NOT NULL,
                notes TEXT
            );",
        )
        .unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn
    }

    fn seed_pizza_menu(conn: &Connection) {
        crate::orders::create_product(
            conn,
            ProductInput {
                name: "Margherita".into(),
                description: None,
                base_price_cents: 1500,
                active: true,
                sort_order: 0,
                extras: vec![
                    ProductExtraInput {
                        name: "Bacon".into(),
                        price_delta_cents: 250,
                        sort_order: 0,
                    },
                    ProductExtraInput {
                        name: "Extra cheese".into(),
                        price_delta_cents: 200,
                        sort_order: 1,
                    },
                ],
            },
        )
        .unwrap();
    }

    fn cfg() -> OrdersConfig {
        OrdersConfig {
            enabled: true,
            currency_symbol: "$".into(),
        }
    }

    #[test]
    fn parse_list_products() {
        let calls = extract_order_tool_calls("<TOOL>list_products</TOOL>");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].as_ref().unwrap(), &OrderToolCall::ListProducts);
    }

    #[test]
    fn parse_add_item_with_extras_and_notes() {
        let calls = extract_order_tool_calls(
            "<TOOL>add_item product=\"Margherita\" quantity=2 extras=\"Bacon, Extra cheese\" notes=\"well done\"</TOOL>",
        );
        assert_eq!(calls.len(), 1);
        match calls[0].as_ref().unwrap() {
            OrderToolCall::AddItem {
                product,
                quantity,
                extras,
                notes,
            } => {
                assert_eq!(product, "Margherita");
                assert_eq!(*quantity, 2);
                assert_eq!(extras, &vec!["Bacon".to_string(), "Extra cheese".into()]);
                assert_eq!(notes.as_deref(), Some("well done"));
            }
            other => panic!("expected AddItem, got {:?}", other),
        }
    }

    #[test]
    fn parse_place_order_minimal() {
        let calls = extract_order_tool_calls("<TOOL>place_order</TOOL>");
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0].as_ref().unwrap(),
            OrderToolCall::PlaceOrder { .. }
        ));
    }

    #[test]
    fn calendar_tool_in_same_reply_is_ignored_by_orders_parser() {
        let raw = "<TOOL>list_services</TOOL><TOOL>list_products</TOOL>";
        let calls = extract_order_tool_calls(raw);
        // Only the orders verb should be returned; calendar tools belong
        // to the calendar parser.
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].as_ref().unwrap(), &OrderToolCall::ListProducts);
    }

    #[test]
    fn add_item_then_place_order_persists_and_clears_cart() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let calls = extract_order_tool_calls(
            "<TOOL>add_item product=\"Margherita\" quantity=2 extras=\"Bacon\"</TOOL>\n\
             <TOOL>place_order customer_name=\"Lance\" customer_phone=\"+61432\"</TOOL>",
        );
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", Some("+61432"));
        assert_eq!(results.len(), 2);
        assert!(matches!(results[0], OrderToolResult::ItemAdded { .. }));
        match &results[1] {
            OrderToolResult::Placed {
                order_id,
                item_count,
                total_cents,
            } => {
                assert!(*order_id > 0);
                assert_eq!(*item_count, 1);
                // 2 * (1500 + 250) = 3500
                assert_eq!(*total_cents, 3500);
            }
            other => panic!("expected Placed, got {:?}", other),
        }
    }

    #[test]
    fn unknown_extra_fails_with_helpful_reason() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let calls = extract_order_tool_calls(
            "<TOOL>add_item product=\"Margherita\" quantity=1 extras=\"Truffle\"</TOOL>",
        );
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", None);
        match &results[0] {
            OrderToolResult::AddItemFailed { reason, .. } => {
                assert!(reason.contains("no extra"));
                assert!(reason.contains("Truffle"));
            }
            other => panic!("expected AddItemFailed, got {:?}", other),
        }
    }

    #[test]
    fn unknown_product_fails_when_catalogue_non_empty() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let calls = extract_order_tool_calls("<TOOL>add_item product=\"Sushi\" quantity=1</TOOL>");
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", None);
        assert!(matches!(results[0], OrderToolResult::AddItemFailed { .. }));
    }

    #[test]
    fn place_order_without_items_fails() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let calls = extract_order_tool_calls("<TOOL>place_order customer_name=\"Lance\"</TOOL>");
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", None);
        assert!(matches!(results[0], OrderToolResult::PlaceFailed { .. }));
    }

    #[test]
    fn cancel_unknown_order_id_fails_cleanly() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let calls = extract_order_tool_calls("<TOOL>cancel_order order_id=999</TOOL>");
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", None);
        match &results[0] {
            OrderToolResult::CancelFailed { order_id, reason } => {
                assert_eq!(*order_id, 999);
                assert!(reason.contains("no order with id"));
            }
            other => panic!("expected CancelFailed, got {:?}", other),
        }
    }

    #[test]
    fn cancel_other_callers_order_is_blocked() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        // Place an order owned by +61432000000.
        let setup = extract_order_tool_calls(
            "<TOOL>add_item product=\"Margherita\" quantity=1</TOOL>\n\
             <TOOL>place_order customer_name=\"Lance\"</TOOL>",
        );
        let setup_results = execute_order_tools(&conn, &cfg(), setup, "sms", Some("+61432000000"));
        let order_id = match &setup_results[1] {
            OrderToolResult::Placed { order_id, .. } => *order_id,
            other => panic!("expected Placed, got {:?}", other),
        };
        // A different caller (+61999...) tries to cancel it. The
        // privacy gate should reject without touching the row.
        let calls =
            extract_order_tool_calls(&format!("<TOOL>cancel_order order_id={}</TOOL>", order_id));
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", Some("+61999999999"));
        assert!(matches!(results[0], OrderToolResult::CancelFailed { .. }));
        // Order should still be pending, not cancelled.
        let o = crate::orders::get_order(&conn, order_id).unwrap().unwrap();
        assert_eq!(o.status, "pending");
    }

    #[test]
    fn cancel_own_order_succeeds() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let setup = extract_order_tool_calls(
            "<TOOL>add_item product=\"Margherita\" quantity=1</TOOL>\n\
             <TOOL>place_order customer_name=\"Lance\"</TOOL>",
        );
        let setup_results = execute_order_tools(&conn, &cfg(), setup, "sms", Some("+61432000000"));
        let order_id = match &setup_results[1] {
            OrderToolResult::Placed { order_id, .. } => *order_id,
            other => panic!("expected Placed, got {:?}", other),
        };
        let calls =
            extract_order_tool_calls(&format!("<TOOL>cancel_order order_id={}</TOOL>", order_id));
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", Some("+61432000000"));
        assert!(matches!(results[0], OrderToolResult::Cancelled { .. }));
        let o = crate::orders::get_order(&conn, order_id).unwrap().unwrap();
        assert_eq!(o.status, "cancelled");
    }

    #[test]
    fn trusted_phone_overrides_model_supplied_phone() {
        let conn = fresh_db();
        seed_pizza_menu(&conn);
        let calls = extract_order_tool_calls(
            "<TOOL>add_item product=\"Margherita\" quantity=1</TOOL>\n\
             <TOOL>place_order customer_name=\"Lance\" customer_phone=\"+61999999999\"</TOOL>",
        );
        let results = execute_order_tools(&conn, &cfg(), calls, "sms", Some("+61432000000"));
        match &results[1] {
            OrderToolResult::Placed { order_id, .. } => {
                let o = crate::orders::get_order(&conn, *order_id).unwrap().unwrap();
                assert_eq!(o.customer_phone.as_deref(), Some("+61432000000"));
            }
            other => panic!("expected Placed, got {:?}", other),
        }
    }
}
