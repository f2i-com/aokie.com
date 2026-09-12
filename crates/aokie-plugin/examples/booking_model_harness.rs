//! JSON-lines adapter for local-model integration tests. Uses the production
//! prompt, marker parser and call-scoped validator; never touches phone hardware.
use std::io::{self, BufRead};
use serde_json::{json, Value};
fn main() {
    for line in io::stdin().lock().lines() {
        let input: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let today = chrono::NaiveDate::parse_from_str(input["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        let history = input["history"].as_array().unwrap();
        let result = if let Some(reply) = input["reply"].as_str() {
            let callers: Vec<String> = history.iter().rev().filter(|m| m["role"] == "user")
                .filter_map(|m| m["content"].as_str()).map(str::to_string).collect();
            let assistants: Vec<String> = history.iter().rev().filter(|m| m["role"] == "assistant")
                .filter_map(|m| m["content"].as_str()).map(str::to_string).collect();
            match aokie_plugin::conversation_policy::parse_appointment_marker(reply) {
                None => json!({"requested":false}),
                Some(args) => match args.and_then(|args| aokie_plugin::realtime_appointment::validate_with_readback(
                    &args, input["callId"].as_str().unwrap(), Some((history.len() as u32, callers.first().map(String::as_str).unwrap_or(""))),
                    &callers, &assistants, today)) {
                    Ok(r) => json!({"requested":true,"event":{"requestId":r.request_id,"callId":input["callId"],"from":"","callerName":r.caller_name,"service":r.service,"date":r.date,"time":r.time,"agreementTurn":r.agreement_turn,"at":chrono::Utc::now().to_rfc3339()}}),
                    Err(e) => json!({"requested":false,"error":e}),
                }
            }
        } else {
            json!({"prompt": format!("You are Aokie, a concise phone receptionist. Ask one question at a time.{}", aokie_plugin::conversation_policy::context(today, history, false, false))})
        };
        println!("{}", result);
    }
}
