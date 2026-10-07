//! Local token estimates for providers without a usable count endpoint.
//!
//! These are deliberately estimates, not tokenizer results. Count prompt text and
//! tool schemas without serializing the whole request, and give media a bounded
//! allowance instead of treating its base64 or URL as millions of text tokens.
//! Exact media cost depends on model, dimensions, pages, or duration, which a
//! remote reference does not provide. The API labels this fallback as estimated.

use serde_json::Value;

const IMAGE_ALLOWANCE: u64 = 1600;
const OTHER_MEDIA_ALLOWANCE: u64 = 2048;

fn text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4).saturating_add(non_ascii)
}

fn sum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0, u64::saturating_add)
}

/// JSON inside tool arguments, results, and schemas is actual prompt content.
/// Do not discard keys such as `data` or `signature` inside that user data.
fn json_tokens(value: &Value) -> u64 {
    match value {
        Value::String(text) => text_tokens(text),
        Value::Array(values) => sum(values.iter().map(json_tokens)).saturating_add(values.len() as u64 + 1),
        Value::Object(values) => {
            sum(values.iter().map(|(key, value)| text_tokens(key).saturating_add(json_tokens(value)).saturating_add(2)))
        }
        Value::Null => 0,
        _ => 1,
    }
}

fn aliased<'a>(value: &'a Value, camel: &str, snake: &str) -> &'a Value {
    value.get(camel).filter(|v| !v.is_null()).or_else(|| value.get(snake)).unwrap_or(&Value::Null)
}

fn media_tokens(mime: Option<&str>) -> u64 {
    if mime.is_some_and(|mime| mime.starts_with("image/")) { IMAGE_ALLOWANCE } else { OTHER_MEDIA_ALLOWANCE }
}

fn content_tokens(value: &Value) -> u64 {
    match value {
        Value::String(text) => text_tokens(text),
        Value::Array(values) => sum(values.iter().map(content_tokens)),
        Value::Object(_) => {
            let kind = value["type"].as_str().unwrap_or_default();
            match kind {
                "image" | "image_url" | "input_image" => return IMAGE_ALLOWANCE,
                "input_audio" | "audio" | "video" | "input_video" | "input_file" => return OTHER_MEDIA_ALLOWANCE,
                "document" => {
                    let source = &value["source"];
                    return match source["type"].as_str() {
                        Some("text") => content_tokens(&source["data"]),
                        Some("content") => content_tokens(&source["content"]),
                        _ => OTHER_MEDIA_ALLOWANCE,
                    };
                }
                "text" | "input_text" | "output_text" | "summary_text" => return content_tokens(&value["text"]),
                "refusal" => return content_tokens(&value["refusal"]),
                "thinking" => return content_tokens(&value["thinking"]),
                "redacted_thinking" => return 0,
                "reasoning" => return content_tokens(&value["summary"]),
                "tool_use" => return json_tokens(&value["input"]).saturating_add(content_tokens(&value["name"])),
                "tool_result" => return content_tokens(&value["content"]).saturating_add(4),
                "function_call" => {
                    return content_tokens(&value["arguments"]).saturating_add(content_tokens(&value["name"]));
                }
                "custom_tool_call" => {
                    return content_tokens(&value["input"]).saturating_add(content_tokens(&value["name"]));
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let output = &value["output"];
                    return if output.is_object() { json_tokens(output) } else { content_tokens(output) };
                }
                _ => {}
            }
            for (camel, snake) in [("inlineData", "inline_data"), ("fileData", "file_data")] {
                let data = aliased(value, camel, snake);
                if data.is_object() {
                    return media_tokens(aliased(data, "mimeType", "mime_type").as_str());
                }
            }
            if value.get("role").is_some() || kind == "message" {
                return content_tokens(&value["content"])
                    .saturating_add(content_tokens(&value["parts"]))
                    .saturating_add(json_tokens(&value["tool_calls"]))
                    .saturating_add(content_tokens(&value["reasoning_content"]))
                    .saturating_add(4);
            }
            // Gemini text parts and systemInstruction omit a type or role.
            if let Some(text) = value.get("text").filter(|text| text.is_string()) {
                return content_tokens(text);
            }
            if let Some(parts) = value.get("parts") {
                return content_tokens(parts);
            }
            let call = aliased(value, "functionCall", "function_call");
            if call.is_object() {
                return json_tokens(&call["args"]).saturating_add(content_tokens(&call["name"]));
            }
            let response = aliased(value, "functionResponse", "function_response");
            if response.is_object() {
                return json_tokens(&response["response"])
                    .saturating_add(content_tokens(&response["parts"]))
                    .saturating_add(content_tokens(&response["name"]));
            }
            json_tokens(value)
        }
        _ => json_tokens(value),
    }
}

pub fn estimate_tokens(body: &Value) -> u64 {
    let prompt = sum(["system", "messages", "contents", "input", "instructions"]
        .into_iter()
        .map(|key| content_tokens(&body[key])));
    prompt
        .saturating_add(content_tokens(aliased(body, "systemInstruction", "system_instruction")))
        .saturating_add(json_tokens(&body["tools"]))
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encoded_image_size_does_not_become_text_tokens() {
        let estimate = |data: String| {
            estimate_tokens(&json!({"messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}
            ]}]}))
        };
        assert_eq!(estimate("AAAA".into()), estimate("AAAA".repeat(1_000_000)));
        assert!(estimate("AAAA".into()) < 10_000);
        let remote = |url: String| {
            estimate_tokens(&json!({"input":[{"role":"user","content":[
                {"type":"input_image","image_url":url}
            ]}]}))
        };
        assert_eq!(
            remote("https://example.test/a.png".into()),
            remote(format!("data:image/png;base64,{}", "A".repeat(100_000)))
        );
    }

    #[test]
    fn gemini_instructions_tools_and_each_image_contribute() {
        let base = json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]});
        let mut body = base.clone();
        body["system_instruction"] = json!({"parts":[{"text":"stable instructions ".repeat(100)}]});
        body["tools"] = json!([{"functionDeclarations":[{"name":"lookup","description":"description ".repeat(100),
            "parameters":{"type":"OBJECT","properties":{"q":{"type":"STRING"}}}}]}]);
        assert!(estimate_tokens(&body) > estimate_tokens(&base) + 500);
        let before = estimate_tokens(&body);
        body["contents"][0]["parts"].as_array_mut().unwrap().extend([
            json!({"inline_data":{"mime_type":"image/png","data":"A".repeat(100_000)}}),
            json!({"fileData":{"mimeType":"image/jpeg","fileUri":"https://example.test/b.jpg"}}),
        ]);
        assert_eq!(estimate_tokens(&body) - before, 2 * IMAGE_ALLOWANCE);
    }

    #[test]
    fn tool_data_and_text_documents_are_counted_but_opaque_signatures_are_not() {
        let result =
            |n| estimate_tokens(&json!({"input":[{"type":"function_call_output","output":{"data":"x".repeat(n)}}]}));
        assert!(result(4000) > result(4) + 900);
        let document = json!({"messages":[{"role":"user","content":[
            {"type":"document","source":{"type":"text","data":"text ".repeat(1000)}}
        ]}]} );
        assert!(estimate_tokens(&document) > 1000);
        let reasoning = |n| {
            estimate_tokens(&json!({"input":[{"type":"reasoning","summary":[
            {"type":"summary_text","text":"checking"}],"encrypted_content":"A".repeat(n)}]}))
        };
        assert_eq!(reasoning(10), reasoning(100_000));
        assert!(estimate_tokens(&json!({"input":"https://example.test/".repeat(100)})) > 400);
    }
}
