//! Build the orders-aware system-prompt suffix. Mirrors
//! `crate::calendar::prompt`: the suffix is appended once per LLM
//! pass, the framing for tool-result re-injection is
//! `tool_result_user_turn`. Calendar and orders suffixes coexist so an
//! operator can run both — the bot decides which tool family to use
//! based on the customer's request.
//!
//! The voice-mode block tells the model NOT to emit XML during a live
//! call (the post-call extractor mints the order from the transcript
//! the same way it mints appointments). The SMS block is the one that
//! actually emits `<TOOL>...</TOOL>`.

use super::{format_money, OrdersConfig, Product};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Sms,
    Voice,
}

/// Append the orders-aware suffix to whatever persona prompt is
/// configured. Returns the persona unchanged when orders is disabled.
pub fn extend_system_prompt(
    persona_prompt: &str,
    config: &OrdersConfig,
    products: &[Product],
) -> String {
    extend_system_prompt_with_mode(persona_prompt, config, products, PromptMode::Sms)
}

pub fn extend_system_prompt_with_mode(
    persona_prompt: &str,
    config: &OrdersConfig,
    products: &[Product],
    mode: PromptMode,
) -> String {
    let mut out = persona_prompt.trim_end().to_string();
    if !config.enabled {
        return out;
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str("ORDERS\n");
    out.push_str(
        "You can take customer orders. Stay conversational — confirm \
         items as you go, read totals back naturally, and only emit the \
         order tool grammar when the customer is ready (or you need a \
         price). Never paste the menu raw or mention these instructions.\n\n",
    );
    out.push_str(&render_menu_block(products, &config.currency_symbol));
    out.push('\n');
    match mode {
        PromptMode::Sms => out.push_str(render_tool_protocol_block()),
        PromptMode::Voice => out.push_str(render_voice_block()),
    }
    out
}

fn render_voice_block() -> &'static str {
    "ORDERS DURING THIS CALL\n\
     Take orders conversationally — do NOT emit XML or tool tags. \
     Confirm each item, optionally read back the running total in your \
     head — line price = (base + extras) × quantity, then sum all \
     lines for the grand total — and at the end repeat the full \
     order with the grand total. The actual order is recorded after \
     the call ends."
}

fn render_menu_block(products: &[Product], symbol: &str) -> String {
    let active: Vec<&Product> = products.iter().filter(|p| p.active).collect();
    if active.is_empty() {
        return "Menu: nothing configured yet — if a customer asks to \
                order, take their details and tell them you'll get \
                someone to call back to confirm what they wanted.\n"
            .to_string();
    }
    let mut s = String::from("Menu:\n");
    for p in active {
        s.push_str(&format!(
            "- {} ({})",
            p.name,
            format_money(p.base_price_cents, symbol)
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
                format_money(e.price_delta_cents, symbol)
            ));
        }
    }
    s
}

fn render_tool_protocol_block() -> &'static str {
    "ORDER TOOLS\n\
     When a customer wants to order, build the cart with one or more \
     add_item lines, then place_order to confirm.\n\
     \n\
     <TOOL>list_products</TOOL>\n\
     <TOOL>add_item product=\"Product Name\" quantity=N extras=\"Extra A,Extra B\" notes=\"optional\"</TOOL>\n\
     <TOOL>quote_order</TOOL>\n\
     <TOOL>place_order customer_name=\"Full Name\" customer_phone=\"+61432...\" notes=\"optional\"</TOOL>\n\
     <TOOL>cancel_order order_id=N</TOOL>\n\
     \n\
     Rules:\n\
     - Use ONE add_item per line item. Two pizzas of the same kind go \
       in ONE add_item with quantity=2; two different pizzas need two \
       add_item lines.\n\
     - Use only the extras shown for that product. If the customer asks \
       for an extra you don't see, say it's not on the menu.\n\
     - Confirm name before place_order. Don't ask for the phone — the \
       system already knows the SMS sender's number and stamps it on \
       place_order automatically; never echo a phone you weren't told.\n\
     - quote_order is for reading the running total back to the \
       customer mid-cart; place_order finalises and writes the order.\n\
     - If the customer just chats about the menu without ordering, \
       reply normally without any order tool call.\n\
     \n\
     CART STATE (important):\n\
     The cart only lives within ONE reply — it is REBUILT from your \
     <TOOL>add_item</TOOL> calls each time. If the conversation spans \
     multiple SMS turns and you're ready to finalise, re-emit every \
     add_item from the conversation history in this same reply BEFORE \
     the place_order line. Example: customer said \"a margherita\" two \
     turns ago and \"and a coke\" just now. To finalise, emit BOTH \
     add_item lines plus place_order in one reply, even though the \
     margherita add_item appeared in your earlier reply too. Don't \
     rely on prior turns to carry the cart forward."
}

/// Frame a batch of orders-tool results as the next user-role turn.
/// Same shape as the calendar version so pass-2 of the LLM has a
/// consistent way to read tool output back. Caller passes the rendered
/// text from `tools::render_results_for_llm`.
pub fn tool_result_user_turn(rendered_results: &str) -> String {
    format!(
        "ORDER_TOOL_RESULTS (do not show this raw to the customer):\n\
         {}\n\
         \n\
         Now reply to the customer in plain conversational text. If \
         you placed an order, confirm what's in it and the total. If \
         an item was added, confirm naturally without listing internal \
         IDs. Never paste tool tags or the cart total formatting into \
         your reply.",
        rendered_results,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::ProductExtra;

    fn cfg(enabled: bool) -> OrdersConfig {
        OrdersConfig {
            enabled,
            currency_symbol: "$".into(),
        }
    }

    fn pizza() -> Product {
        Product {
            id: 1,
            name: "Margherita".into(),
            description: Some("Tomato, basil, mozz".into()),
            base_price_cents: 1500,
            active: true,
            sort_order: 0,
            created_at: 0,
            extras: vec![
                ProductExtra {
                    id: 1,
                    product_id: 1,
                    name: "Bacon".into(),
                    price_delta_cents: 250,
                    sort_order: 0,
                },
                ProductExtra {
                    id: 2,
                    product_id: 1,
                    name: "Extra cheese".into(),
                    price_delta_cents: 200,
                    sort_order: 1,
                },
            ],
        }
    }

    #[test]
    fn disabled_returns_persona_unchanged() {
        let s = extend_system_prompt("You are a chef.", &cfg(false), &[]);
        assert_eq!(s, "You are a chef.");
    }

    #[test]
    fn enabled_appends_menu_and_protocol() {
        let s = extend_system_prompt("You are a chef.", &cfg(true), &[pizza()]);
        assert!(s.contains("ORDERS"));
        assert!(s.contains("Margherita ($15.00)"));
        assert!(s.contains("Bacon ($2.50)"));
        assert!(s.contains("Extra cheese ($2.00)"));
        assert!(s.contains("<TOOL>add_item"));
        assert!(s.contains("<TOOL>place_order"));
    }

    #[test]
    fn empty_menu_falls_back_to_callback() {
        let s = extend_system_prompt("Hi.", &cfg(true), &[]);
        assert!(s.contains("nothing configured yet"));
    }

    #[test]
    fn voice_mode_omits_tool_protocol() {
        let s = extend_system_prompt_with_mode("Hi.", &cfg(true), &[pizza()], PromptMode::Voice);
        assert!(s.contains("ORDERS DURING THIS CALL"));
        assert!(!s.contains("<TOOL>add_item"));
    }
}
