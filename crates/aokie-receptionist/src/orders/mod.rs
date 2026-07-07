//! Orders system — products + extras + orders + order_items. Companion
//! to the calendar module; same shape (typed CRUD, transactional writes,
//! `BookingError`-style typed errors so the bot path can phrase nice
//! error messages instead of leaking raw SQL strings).
//!
//! Money is stored and computed in integer cents throughout so we never
//! round-trip through f64. The frontend displays as `$N.NN`.
//!
//! Schema lives in `database::migrate_v6`. The bot integration calls
//! `list_products`, `add_item_to_batch`, `place_order` — never SQL
//! directly, so the same call shape works from SMS auto-reply,
//! post-call extraction, and the manual UI.

#![allow(dead_code)] // wired in across the orders integration phases

pub mod prompt;
pub mod tools;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

// ============================================================================
// Settings (mirror the JSON shape the frontend writes via save_config_to_file)
// ============================================================================

/// Orders slice of `AppConfig`. Optional in the parent struct so an
/// older config.json without an orders section round-trips cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrdersConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Currency symbol shown in receipts and on the UI. Pure cosmetic —
    /// all maths happens in cents and the bot rounds in user-facing
    /// strings via `format_money`.
    #[serde(default = "default_currency_symbol", rename = "currencySymbol")]
    pub currency_symbol: String,
}

impl Default for OrdersConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            currency_symbol: default_currency_symbol(),
        }
    }
}

fn default_currency_symbol() -> String {
    "$".to_string()
}

// ============================================================================
// Products + extras CRUD
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Product {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub base_price_cents: i64,
    pub active: bool,
    pub sort_order: i64,
    pub created_at: i64,
    /// Loaded eagerly when fetching a single product / via
    /// `list_products_with_extras`; empty when fetched bare.
    #[serde(default)]
    pub extras: Vec<ProductExtra>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProductExtra {
    pub id: i64,
    pub product_id: i64,
    pub name: String,
    pub price_delta_cents: i64,
    pub sort_order: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductInput {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub base_price_cents: i64,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub sort_order: i64,
    /// Replace-style extras list — when this comes through `update_product`,
    /// the existing extras are wiped and replaced with this list.
    /// `create_product` follows the same pattern.
    #[serde(default)]
    pub extras: Vec<ProductExtraInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductExtraInput {
    pub name: String,
    #[serde(default)]
    pub price_delta_cents: i64,
    #[serde(default)]
    pub sort_order: i64,
}

fn default_true() -> bool {
    true
}

pub fn list_products(conn: &Connection, only_active: bool) -> rusqlite::Result<Vec<Product>> {
    let sql = if only_active {
        "SELECT id, name, description, base_price_cents, active, sort_order, created_at \
         FROM products WHERE active = 1 ORDER BY sort_order, name"
    } else {
        "SELECT id, name, description, base_price_cents, active, sort_order, created_at \
         FROM products ORDER BY sort_order, name"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], row_to_product)?;
    rows.collect()
}

/// Fetch products and inline their extras in one trip. Used by both
/// the Settings UI (operator wants to see/edit the catalog) and the
/// bot's prompt builder (so the model can quote prices including
/// likely add-ons).
pub fn list_products_with_extras(
    conn: &Connection,
    only_active: bool,
) -> rusqlite::Result<Vec<Product>> {
    let mut products = list_products(conn, only_active)?;
    let extras_by_product = list_all_extras_grouped(conn)?;
    for p in &mut products {
        if let Some(v) = extras_by_product.get(&p.id) {
            p.extras = v.clone();
        }
    }
    Ok(products)
}

pub fn get_product(conn: &Connection, id: i64) -> rusqlite::Result<Option<Product>> {
    let mut p = match conn
        .query_row(
            "SELECT id, name, description, base_price_cents, active, sort_order, created_at \
             FROM products WHERE id = ?1",
            params![id],
            row_to_product,
        )
        .optional()?
    {
        Some(p) => p,
        None => return Ok(None),
    };
    p.extras = list_extras_for_product(conn, id)?;
    Ok(Some(p))
}

pub fn create_product(conn: &Connection, input: ProductInput) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;
    conn.execute(
        "INSERT INTO products (name, description, base_price_cents, active, sort_order, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            input.name,
            input.description,
            input.base_price_cents,
            input.active as i64,
            input.sort_order,
            now_unix(),
        ],
    )?;
    let product_id = conn.last_insert_rowid();
    insert_extras_replacing(conn, product_id, &input.extras)?;
    tx.commit()?;
    Ok(product_id)
}

pub fn update_product(conn: &Connection, id: i64, input: ProductInput) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    conn.execute(
        "UPDATE products SET name = ?1, description = ?2, base_price_cents = ?3, \
         active = ?4, sort_order = ?5 WHERE id = ?6",
        params![
            input.name,
            input.description,
            input.base_price_cents,
            input.active as i64,
            input.sort_order,
            id,
        ],
    )?;
    insert_extras_replacing(conn, id, &input.extras)?;
    tx.commit()?;
    Ok(())
}

pub fn deactivate_product(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE products SET active = 0 WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn delete_product_hard(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    // FK on order_items.product_id is ON DELETE SET NULL, so historical
    // rows keep their denormalised name. Extras CASCADE.
    conn.execute("DELETE FROM products WHERE id = ?1", params![id])?;
    Ok(())
}

fn insert_extras_replacing(
    conn: &Connection,
    product_id: i64,
    extras: &[ProductExtraInput],
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM product_extras WHERE product_id = ?1",
        params![product_id],
    )?;
    for (idx, e) in extras.iter().enumerate() {
        let order = if e.sort_order != 0 {
            e.sort_order
        } else {
            idx as i64
        };
        conn.execute(
            "INSERT INTO product_extras (product_id, name, price_delta_cents, sort_order) \
             VALUES (?1, ?2, ?3, ?4)",
            params![product_id, e.name, e.price_delta_cents, order],
        )?;
    }
    Ok(())
}

fn list_extras_for_product(
    conn: &Connection,
    product_id: i64,
) -> rusqlite::Result<Vec<ProductExtra>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, name, price_delta_cents, sort_order \
         FROM product_extras WHERE product_id = ?1 ORDER BY sort_order, name",
    )?;
    let rows = stmt.query_map(params![product_id], row_to_extra)?;
    rows.collect()
}

fn list_all_extras_grouped(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<i64, Vec<ProductExtra>>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, name, price_delta_cents, sort_order \
         FROM product_extras ORDER BY sort_order, name",
    )?;
    let rows = stmt.query_map([], row_to_extra)?;
    let mut out: std::collections::HashMap<i64, Vec<ProductExtra>> =
        std::collections::HashMap::new();
    for r in rows {
        let r = r?;
        out.entry(r.product_id).or_default().push(r);
    }
    Ok(out)
}

fn row_to_product(row: &rusqlite::Row<'_>) -> rusqlite::Result<Product> {
    Ok(Product {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        base_price_cents: row.get(3)?,
        active: row.get::<_, i64>(4)? != 0,
        sort_order: row.get(5)?,
        created_at: row.get(6)?,
        extras: Vec::new(),
    })
}

fn row_to_extra(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProductExtra> {
    Ok(ProductExtra {
        id: row.get(0)?,
        product_id: row.get(1)?,
        name: row.get(2)?,
        price_delta_cents: row.get(3)?,
        sort_order: row.get(4)?,
    })
}

// ============================================================================
// Orders
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Order {
    pub id: i64,
    pub customer_phone: Option<String>,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    pub source: String,
    pub total_cents: i64,
    pub created_at: i64,
    /// Eagerly loaded by `get_order` and `list_orders`.
    #[serde(default)]
    pub items: Vec<OrderItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderItem {
    pub id: i64,
    pub order_id: i64,
    pub product_id: Option<i64>,
    pub product_name: String,
    pub base_price_cents: i64,
    pub quantity: i64,
    /// Decoded from extras_json. Each entry: (extra name, price delta).
    pub extras: Vec<AppliedExtra>,
    pub line_total_cents: i64,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedExtra {
    pub name: String,
    pub price_delta_cents: i64,
}

/// Input shape for adding a single line item, before pricing is
/// computed. The bot path converts each `<TOOL>add_item ...</TOOL>`
/// into one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderItemInput {
    /// `None` for ad-hoc entries (free-form orders, mostly used by the
    /// manual UI when an operator hasn't built a product catalogue yet).
    pub product_id: Option<i64>,
    pub product_name: String,
    pub base_price_cents: i64,
    pub quantity: i64,
    /// Names + deltas to record as the applied extras. The pricing
    /// helper trusts these — the bot integration looks them up against
    /// the product's catalogue extras before populating, so a customer
    /// can't slip in a free-of-charge "Extra cheese" by typing it out.
    #[serde(default)]
    pub extras: Vec<AppliedExtra>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderInput {
    #[serde(default)]
    pub customer_phone: Option<String>,
    #[serde(default)]
    pub customer_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// "sms" | "call" | "manual". Defaults to "manual".
    #[serde(default = "default_source")]
    pub source: String,
    pub items: Vec<OrderItemInput>,
}

fn default_source() -> String {
    "manual".to_string()
}

/// Persist a complete order with items in one transaction. Computes
/// per-line totals and the order grand total from the supplied prices
/// (caller is expected to have already validated extras against the
/// product catalogue if it wants to enforce price integrity).
pub fn place_order(conn: &Connection, input: OrderInput) -> Result<i64, OrderError> {
    if input.items.is_empty() {
        return Err(OrderError::Invalid("order needs at least one item".into()));
    }
    for item in &input.items {
        if item.quantity <= 0 {
            return Err(OrderError::Invalid(format!(
                "quantity must be positive (got {} for {:?})",
                item.quantity, item.product_name
            )));
        }
        if item.base_price_cents < 0 {
            return Err(OrderError::Invalid(format!(
                "negative base price for {:?}",
                item.product_name
            )));
        }
        // Extras can carry negative deltas (a "no-cheese -50¢"
        // modifier is legitimate), but the unit price after applying
        // them must still be non-negative — otherwise the model
        // could place an order whose line total is negative and the
        // operator would owe the customer money. Catch this here
        // rather than after the saturating math in compute_line_total
        // so the error message points at the offending item.
        let extras_total: i64 = item.extras.iter().map(|e| e.price_delta_cents).sum();
        let unit = item.base_price_cents.saturating_add(extras_total);
        if unit < 0 {
            return Err(OrderError::Invalid(format!(
                "extras for {:?} push unit price negative ({}¢ + {}¢ extras = {}¢)",
                item.product_name, item.base_price_cents, extras_total, unit
            )));
        }
    }
    let tx = conn.unchecked_transaction().map_err(OrderError::Sql)?;
    let now = now_unix();
    let mut total_cents: i64 = 0;
    conn.execute(
        "INSERT INTO orders (customer_phone, customer_name, notes, status, source, \
         total_cents, created_at) VALUES (?1, ?2, ?3, 'pending', ?4, 0, ?5)",
        params![
            input.customer_phone,
            input.customer_name,
            input.notes,
            input.source,
            now,
        ],
    )
    .map_err(OrderError::Sql)?;
    let order_id = conn.last_insert_rowid();
    for item in &input.items {
        let line_total = compute_line_total(item);
        total_cents = total_cents.saturating_add(line_total);
        let extras_json = serde_json::to_string(&item.extras)
            .map_err(|e| OrderError::Invalid(format!("encode extras: {}", e)))?;
        conn.execute(
            "INSERT INTO order_items (order_id, product_id, product_name, \
             base_price_cents, quantity, extras_json, line_total_cents, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                order_id,
                item.product_id,
                item.product_name,
                item.base_price_cents,
                item.quantity,
                extras_json,
                line_total,
                item.notes,
            ],
        )
        .map_err(OrderError::Sql)?;
    }
    conn.execute(
        "UPDATE orders SET total_cents = ?1 WHERE id = ?2",
        params![total_cents, order_id],
    )
    .map_err(OrderError::Sql)?;
    tx.commit().map_err(OrderError::Sql)?;
    Ok(order_id)
}

/// `(base + sum(extras)) * quantity`. Saturating to clamp pathological
/// inputs into a representable range — we never actually expect cents
/// values that would overflow i64, but a malicious or buggy caller
/// shouldn't crash the booking path.
pub fn compute_line_total(item: &OrderItemInput) -> i64 {
    let extras_total: i64 = item.extras.iter().map(|e| e.price_delta_cents).sum();
    let unit = item.base_price_cents.saturating_add(extras_total);
    unit.saturating_mul(item.quantity)
}

#[derive(Debug)]
pub enum OrderError {
    Invalid(String),
    Sql(rusqlite::Error),
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::Invalid(msg) => write!(f, "{}", msg),
            OrderError::Sql(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for OrderError {}

pub fn cancel_order(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE orders SET status = 'cancelled' WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn set_order_status(conn: &Connection, id: i64, status: &str) -> Result<(), OrderError> {
    if !matches!(
        status,
        "pending" | "preparing" | "ready" | "completed" | "cancelled"
    ) {
        return Err(OrderError::Invalid(format!("invalid status {:?}", status)));
    }
    conn.execute(
        "UPDATE orders SET status = ?1 WHERE id = ?2",
        params![status, id],
    )
    .map_err(OrderError::Sql)?;
    Ok(())
}

pub fn delete_order(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM orders WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_order(conn: &Connection, id: i64) -> rusqlite::Result<Option<Order>> {
    let mut order = match conn
        .query_row(
            "SELECT id, customer_phone, customer_name, notes, status, source, \
             total_cents, created_at FROM orders WHERE id = ?1",
            params![id],
            row_to_order_bare,
        )
        .optional()?
    {
        Some(o) => o,
        None => return Ok(None),
    };
    order.items = list_items_for_order(conn, id)?;
    Ok(Some(order))
}

/// Fetch the most recent N orders with items inlined. The Orders page
/// uses this; bots use narrower lookups.
pub fn list_recent_orders(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<Order>> {
    let mut stmt = conn.prepare(
        "SELECT id, customer_phone, customer_name, notes, status, source, \
         total_cents, created_at FROM orders ORDER BY created_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], row_to_order_bare)?;
    let mut orders: Vec<Order> = rows.collect::<rusqlite::Result<_>>()?;
    let ids: Vec<i64> = orders.iter().map(|o| o.id).collect();
    let items_by_order = list_items_for_orders(conn, &ids)?;
    for o in &mut orders {
        if let Some(v) = items_by_order.get(&o.id) {
            o.items = v.clone();
        }
    }
    Ok(orders)
}

fn list_items_for_order(conn: &Connection, order_id: i64) -> rusqlite::Result<Vec<OrderItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, order_id, product_id, product_name, base_price_cents, \
         quantity, extras_json, line_total_cents, notes \
         FROM order_items WHERE order_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![order_id], row_to_order_item)?;
    rows.collect()
}

fn list_items_for_orders(
    conn: &Connection,
    order_ids: &[i64],
) -> rusqlite::Result<std::collections::HashMap<i64, Vec<OrderItem>>> {
    let mut out: std::collections::HashMap<i64, Vec<OrderItem>> = std::collections::HashMap::new();
    if order_ids.is_empty() {
        return Ok(out);
    }
    let placeholders: String = std::iter::repeat("?")
        .take(order_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, order_id, product_id, product_name, base_price_cents, \
         quantity, extras_json, line_total_cents, notes FROM order_items \
         WHERE order_id IN ({}) ORDER BY id",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = order_ids
        .iter()
        .map(|i| i as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(params), row_to_order_item)?;
    for r in rows {
        let r = r?;
        out.entry(r.order_id).or_default().push(r);
    }
    Ok(out)
}

fn row_to_order_bare(row: &rusqlite::Row<'_>) -> rusqlite::Result<Order> {
    Ok(Order {
        id: row.get(0)?,
        customer_phone: row.get(1)?,
        customer_name: row.get(2)?,
        notes: row.get(3)?,
        status: row.get(4)?,
        source: row.get(5)?,
        total_cents: row.get(6)?,
        created_at: row.get(7)?,
        items: Vec::new(),
    })
}

fn row_to_order_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<OrderItem> {
    let id: i64 = row.get(0)?;
    let extras_json: String = row.get(6)?;
    // If the JSON is corrupt (manual DB edit, partial migration, etc),
    // we fall back to an empty extras list so the order still renders.
    // BUT log the parse failure: silently empty extras would make the
    // line_total_cents look wrong with no diagnostic, which is worse
    // than a noisy log line at row-fetch time.
    let extras: Vec<AppliedExtra> = match serde_json::from_str(&extras_json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "[orders] corrupted extras_json on order_item id={}: {} (raw len={})",
                id,
                e,
                extras_json.len(),
            );
            Vec::new()
        }
    };
    Ok(OrderItem {
        id,
        order_id: row.get(1)?,
        product_id: row.get(2)?,
        product_name: row.get(3)?,
        base_price_cents: row.get(4)?,
        quantity: row.get(5)?,
        extras,
        line_total_cents: row.get(7)?,
        notes: row.get(8)?,
    })
}

/// Format integer cents as `$N.NN` (or whatever symbol the operator
/// configured). Used in tool result rendering and the LLM-facing
/// receipt — the frontend has its own formatter for the React tree.
pub fn format_money(cents: i64, symbol: &str) -> String {
    let neg = cents < 0;
    let abs = cents.unsigned_abs() as u64;
    let dollars = abs / 100;
    let frac = abs % 100;
    if neg {
        format!("-{}{}.{:02}", symbol, dollars, frac)
    } else {
        format!("{}{}.{:02}", symbol, dollars, frac)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK(status IN ('pending','preparing','ready','completed','cancelled')),
                source TEXT NOT NULL DEFAULT 'manual'
                    CHECK(source IN ('sms','call','manual')),
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

    fn seed_pizza(conn: &Connection) -> i64 {
        create_product(
            conn,
            ProductInput {
                name: "Margherita".into(),
                description: Some("Tomato, mozzarella, basil".into()),
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
        .unwrap()
    }

    #[test]
    fn product_round_trip_with_extras() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let p = get_product(&conn, id).unwrap().unwrap();
        assert_eq!(p.name, "Margherita");
        assert_eq!(p.base_price_cents, 1500);
        assert_eq!(p.extras.len(), 2);
        assert_eq!(p.extras[0].name, "Bacon");
        assert_eq!(p.extras[0].price_delta_cents, 250);
    }

    #[test]
    fn update_product_replaces_extras() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        update_product(
            &conn,
            id,
            ProductInput {
                name: "Margherita".into(),
                description: None,
                base_price_cents: 1600,
                active: true,
                sort_order: 0,
                extras: vec![ProductExtraInput {
                    name: "Olives".into(),
                    price_delta_cents: 150,
                    sort_order: 0,
                }],
            },
        )
        .unwrap();
        let p = get_product(&conn, id).unwrap().unwrap();
        assert_eq!(p.base_price_cents, 1600);
        assert_eq!(p.extras.len(), 1);
        assert_eq!(p.extras[0].name, "Olives");
    }

    #[test]
    fn deactivate_product_hides_from_only_active_list() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        deactivate_product(&conn, id).unwrap();
        let active = list_products(&conn, true).unwrap();
        assert!(active.is_empty());
        let all = list_products(&conn, false).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn place_order_computes_line_and_grand_totals() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let order_id = place_order(
            &conn,
            OrderInput {
                customer_phone: Some("+61432000000".into()),
                customer_name: Some("Lance".into()),
                notes: None,
                source: "manual".into(),
                items: vec![
                    OrderItemInput {
                        product_id: Some(id),
                        product_name: "Margherita".into(),
                        base_price_cents: 1500,
                        quantity: 2,
                        extras: vec![AppliedExtra {
                            name: "Bacon".into(),
                            price_delta_cents: 250,
                        }],
                        notes: None,
                    },
                    OrderItemInput {
                        product_id: Some(id),
                        product_name: "Margherita".into(),
                        base_price_cents: 1500,
                        quantity: 1,
                        extras: vec![],
                        notes: None,
                    },
                ],
            },
        )
        .unwrap();
        let o = get_order(&conn, order_id).unwrap().unwrap();
        // 2 * (1500 + 250) = 3500; 1 * 1500 = 1500; total 5000
        assert_eq!(o.total_cents, 5000);
        assert_eq!(o.items.len(), 2);
        assert_eq!(o.items[0].line_total_cents, 3500);
        assert_eq!(o.items[1].line_total_cents, 1500);
    }

    #[test]
    fn empty_order_is_rejected() {
        let conn = fresh_db();
        let err = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![],
            },
        );
        assert!(matches!(err, Err(OrderError::Invalid(_))));
    }

    #[test]
    fn negative_quantity_rejected() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let err = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![OrderItemInput {
                    product_id: Some(id),
                    product_name: "Margherita".into(),
                    base_price_cents: 1500,
                    quantity: 0,
                    extras: vec![],
                    notes: None,
                }],
            },
        );
        assert!(matches!(err, Err(OrderError::Invalid(_))));
    }

    #[test]
    fn negative_extras_pushing_unit_price_below_zero_rejected() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let err = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![OrderItemInput {
                    product_id: Some(id),
                    product_name: "Margherita".into(),
                    base_price_cents: 1500,
                    quantity: 1,
                    // -1500 (e.g. promo discount) plus a -200 modifier
                    // pushes the unit below zero — rejected.
                    extras: vec![
                        AppliedExtra {
                            name: "no cheese".into(),
                            price_delta_cents: -1500,
                        },
                        AppliedExtra {
                            name: "no sauce".into(),
                            price_delta_cents: -200,
                        },
                    ],
                    notes: None,
                }],
            },
        );
        match err {
            Err(OrderError::Invalid(msg)) => assert!(
                msg.contains("negative"),
                "expected a negative-unit-price error, got: {}",
                msg
            ),
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    #[test]
    fn negative_extras_within_base_are_allowed() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        // Base 1500, -200 modifier — unit stays at 1300, fine.
        let order_id = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![OrderItemInput {
                    product_id: Some(id),
                    product_name: "Margherita".into(),
                    base_price_cents: 1500,
                    quantity: 2,
                    extras: vec![AppliedExtra {
                        name: "no cheese".into(),
                        price_delta_cents: -200,
                    }],
                    notes: None,
                }],
            },
        )
        .expect("place_order should succeed for non-negative unit price");
        let order = get_order(&conn, order_id)
            .expect("get_order")
            .expect("order persisted");
        assert_eq!(order.total_cents, 2600);
    }

    #[test]
    fn cancelled_order_keeps_items_and_total() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let order_id = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![OrderItemInput {
                    product_id: Some(id),
                    product_name: "Margherita".into(),
                    base_price_cents: 1500,
                    quantity: 1,
                    extras: vec![],
                    notes: None,
                }],
            },
        )
        .unwrap();
        cancel_order(&conn, order_id).unwrap();
        let o = get_order(&conn, order_id).unwrap().unwrap();
        assert_eq!(o.status, "cancelled");
        assert_eq!(o.items.len(), 1);
        assert_eq!(o.total_cents, 1500);
    }

    #[test]
    fn set_status_validates() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let order_id = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![OrderItemInput {
                    product_id: Some(id),
                    product_name: "Margherita".into(),
                    base_price_cents: 1500,
                    quantity: 1,
                    extras: vec![],
                    notes: None,
                }],
            },
        )
        .unwrap();
        set_order_status(&conn, order_id, "preparing").unwrap();
        let o = get_order(&conn, order_id).unwrap().unwrap();
        assert_eq!(o.status, "preparing");
        let err = set_order_status(&conn, order_id, "shipped");
        assert!(matches!(err, Err(OrderError::Invalid(_))));
    }

    #[test]
    fn format_money_renders_dollars_and_cents() {
        assert_eq!(format_money(0, "$"), "$0.00");
        assert_eq!(format_money(50, "$"), "$0.50");
        assert_eq!(format_money(1234, "$"), "$12.34");
        assert_eq!(format_money(-150, "$"), "-$1.50");
        assert_eq!(format_money(100, "€"), "€1.00");
    }

    #[test]
    fn delete_product_keeps_historical_order_items_via_set_null() {
        let conn = fresh_db();
        let id = seed_pizza(&conn);
        let order_id = place_order(
            &conn,
            OrderInput {
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
                items: vec![OrderItemInput {
                    product_id: Some(id),
                    product_name: "Margherita".into(),
                    base_price_cents: 1500,
                    quantity: 1,
                    extras: vec![],
                    notes: None,
                }],
            },
        )
        .unwrap();
        delete_product_hard(&conn, id).unwrap();
        let o = get_order(&conn, order_id).unwrap().unwrap();
        assert_eq!(o.items.len(), 1);
        assert!(o.items[0].product_id.is_none());
        assert_eq!(o.items[0].product_name, "Margherita");
    }
}
