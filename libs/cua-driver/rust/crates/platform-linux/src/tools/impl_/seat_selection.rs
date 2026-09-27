use super::*;
use cua_driver_core::action_record::{
    ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
};

fn record(
    effect: ActionEffect,
    delivery: Option<ActualDelivery>,
    delivered_count: Option<u32>,
    detail: &str,
) -> ActionExecutionRecord {
    let mut builder = ActionExecutionRecord::builder(
        effect,
        ActionTransport::LinuxHyprlandAgentSelection,
        RequestedDelivery::Background,
    )
    .detail(detail);
    if let Some(delivery) = delivery {
        builder = builder.actual_delivery(delivery);
    }
    if let Some(count) = delivered_count {
        builder = builder.delivered_count(count);
    }
    builder.build().expect("selection delivery record is valid")
}

pub struct SetAgentSelectionTool;
pub struct ClearAgentSelectionTool;

static SET_SELECTION_DEF: std::sync::LazyLock<ToolDef> = std::sync::LazyLock::new(|| {
    ToolDef {
    name: "set_agent_selection".into(),
    description: "Explicitly own a bounded UTF-8 text selection on this session's isolated Hyprland agent seat for one exact native destination window. Does not replace the human desktop clipboard; the destination must bind wl_data_device on that seat. A compositor offer does not prove paste or application effect.".into(),
    input_schema: json!({"type":"object","required":["session","pid","window_id","text"],"properties":{
        "session": cua_driver_core::tool_schema::session_schema(),
        "pid":{"type":"integer","minimum":1},
        "window_id":{"type":"integer","minimum":1},
        "text":{"type":"string","minLength":1,"description":"1..=900 UTF-8 bytes; replaces only this agent seat's selection, not the shared desktop clipboard."}
    },"additionalProperties":false}),
    read_only: false, destructive: true, idempotent: false, open_world: true,
}
});
static CLEAR_SELECTION_DEF: std::sync::LazyLock<ToolDef> = std::sync::LazyLock::new(|| {
    ToolDef {
    name: "clear_agent_selection".into(),
    description: "Release only the selection this session still owns on its isolated Hyprland agent seat; do not clear any later owner or the desktop clipboard.".into(),
    input_schema: json!({"type":"object","required":["session"],"properties":{
        "session": cua_driver_core::tool_schema::session_schema()
    },"additionalProperties":false}),
    read_only: false, destructive: true, idempotent: true, open_world: false,
}
});

fn refusal(code: &str, detail: impl Into<String>) -> ToolResult {
    let detail = detail.into();
    ToolResult::error(format!("{code}: {detail}"))
        .with_structured(json!({
            "code":code, "detail":detail, "effect":"refused", "selection_scope":"agent_seat",
            "application_result":"unverified"
        }))
        .with_action_record(record(ActionEffect::Refused, None, None, &detail))
}

fn selection_result(result: anyhow::Result<Value>, operation: &str) -> ToolResult {
    match result {
        Ok(value) if value["ok"] == true => {
            let mut structured = json!({
                "route":"agent_seat_selection", "selection_scope":"agent_seat",
                "selection_owner":"agent_session", "effect":"unverifiable",
                "application_result":"unverified", "verified":false,
                "mime_types":["text/plain;charset=utf-8", "text/plain"]
            });
            if operation == "set" {
                structured["selection_owned"] = json!(true);
                structured["offered_to_target"] =
                    json!(value["offered_to_target"].as_bool().unwrap_or(false));
                if let Some(ttl) = value["expires_in_ms"].as_u64() {
                    structured["expires_in_ms"] = json!(ttl);
                }
                ToolResult::text(
                    "Agent-seat text selection is owned; the target application's paste remains unverified.",
                )
                .with_structured(structured)
                .with_action_record(record(
                    ActionEffect::Unverifiable,
                    Some(ActualDelivery::Background),
                    None,
                    "agent-seat selection owned; target paste unverified",
                ))
            } else {
                let Some(cleared) = value["cleared"].as_bool() else {
                    return ToolResult::error(
                        "CLEAR_SELECTION acknowledgement omitted ownership result",
                    )
                    .with_structured(json!({
                        "code":"selection_delivery_unknown", "effect":"unverifiable",
                        "selection_scope":"agent_seat", "application_result":"unverified"
                    }))
                    .with_action_record(record(
                        ActionEffect::Unverifiable,
                        Some(ActualDelivery::Unknown),
                        None,
                        "selection clear acknowledgement incomplete",
                    ));
                };
                structured["cleared"] = json!(cleared);
                ToolResult::text(if cleared {
                    "Released this session's agent-seat selection."
                } else {
                    "No selection owned by this session remained on its agent seat."
                })
                .with_structured(structured)
                .with_action_record(record(
                    ActionEffect::Unverifiable,
                    Some(if cleared {
                        ActualDelivery::Background
                    } else {
                        ActualDelivery::NotApplicable
                    }),
                    None,
                    "agent-seat selection clear result observed; app effect unverified",
                ))
            }
        }
        Ok(value) => {
            let code = value["code"].as_str().unwrap_or("selection_refused");
            let detail = value["detail"]
                .as_str()
                .unwrap_or("selection operation refused");
            let mut result = refusal(code, detail);
            if value["effect"] == "partial" {
                let count = value["delivery"]["delivered_count"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n > 0);
                if let Some(count) = count {
                    result.structured_content.as_mut().unwrap()["effect"] = json!("partial");
                    result.structured_content.as_mut().unwrap()["delivery"] =
                        value["delivery"].clone();
                    result.action_record = Some(record(
                        ActionEffect::Partial,
                        Some(if value["delivery"]["mode"] == "unknown" {
                            ActualDelivery::Unknown
                        } else {
                            ActualDelivery::Background
                        }),
                        Some(count),
                        detail,
                    ));
                } else {
                    result.structured_content.as_mut().unwrap()["effect"] = json!("unverifiable");
                    result.action_record = Some(record(
                        ActionEffect::Unverifiable,
                        Some(ActualDelivery::Unknown),
                        None,
                        detail,
                    ));
                }
            }
            result
        }
        Err(error) => {
            let count = error
                .downcast_ref::<crate::wayland::hyprland_input::DispatchUnknown>()
                .map(|unknown| unknown.acknowledged_phases)
                .unwrap_or(0);
            ToolResult::error(format!(
                "Agent-seat selection delivery is unknown; inspect before retrying: {error}"
            ))
            .with_structured(json!({
                "code":"selection_delivery_unknown", "effect":if count > 0 { "partial" } else { "unverifiable" },
                "delivery":{"mode":"unknown", "acknowledged_phases":count},
                "selection_scope":"agent_seat", "application_result":"unverified"
            }))
            .with_action_record(record(
                if count > 0 { ActionEffect::Partial } else { ActionEffect::Unverifiable },
                Some(ActualDelivery::Unknown),
                (count > 0).then_some(count),
                &error.to_string(),
            ))
        }
    }
}

#[async_trait]
impl Tool for SetAgentSelectionTool {
    fn def(&self) -> &ToolDef {
        &SET_SELECTION_DEF
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let Some(owner) = named_session_cursor_key(&args) else {
            return refusal(
                "selection_lifecycle_required",
                "an authenticated session is required",
            );
        };
        let Some(pid) = args
            .opt_u64("pid")
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid != 0)
        else {
            return refusal("selection_target_required", "exact pid is required");
        };
        let Some(address) = args.opt_u64("window_id").filter(|address| *address != 0) else {
            return refusal("selection_target_required", "exact window_id is required");
        };
        let Some(text) = args.opt_str("text") else {
            return refusal("selection_invalid_payload", "text is required");
        };
        if text.is_empty() || text.len() > 900 || text.contains('\0') {
            return refusal(
                "selection_invalid_payload",
                "text must be 1..=900 UTF-8 bytes without NUL",
            );
        }
        if !crate::wayland::hyprland_input::enabled() {
            return refusal(
                "selection_route_unavailable",
                "native agent-seat selection protocol is unavailable",
            );
        }
        match crate::wayland::hyprland::target_kind(pid, address) {
            Ok(crate::wayland::hyprland::TargetKind::Native) => {}
            Ok(crate::wayland::hyprland::TargetKind::XWayland) => {
                return refusal(
                    "selection_native_target_required",
                    "XWayland cannot bind this native agent-seat selection",
                )
            }
            Err(error) => return refusal("selection_target_unavailable", error.to_string()),
        }
        let (guard, dispatch) = match spawn_isolated_hyprland(&args, move |cancellation| {
            crate::wayland::hyprland_input::execute_selection(
                owner,
                pid,
                address,
                &text,
                cancellation,
            )
        }) {
            Ok(dispatch) => dispatch,
            Err(result) => return result,
        };
        let result = match dispatch.await {
            Ok(result) => selection_result(result, "set"),
            Err(error) => selection_result(
                Err(crate::wayland::hyprland_input::unknown_dispatch(
                    error.into(),
                    0,
                )),
                "set",
            ),
        };
        drop(guard);
        result
    }
}

#[async_trait]
impl Tool for ClearAgentSelectionTool {
    fn def(&self) -> &ToolDef {
        &CLEAR_SELECTION_DEF
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(owner) = named_session_cursor_key(&args) else {
            return refusal(
                "selection_lifecycle_required",
                "an authenticated session is required",
            );
        };
        if !crate::wayland::hyprland_input::enabled() {
            return refusal(
                "selection_route_unavailable",
                "native agent-seat selection protocol is unavailable",
            );
        }
        let (guard, dispatch) = match spawn_isolated_hyprland(&args, move |cancellation| {
            crate::wayland::hyprland_input::clear_selection(owner, cancellation)
        }) {
            Ok(dispatch) => dispatch,
            Err(result) => return result,
        };
        let result = match dispatch.await {
            Ok(result) => selection_result(result, "clear"),
            Err(error) => selection_result(
                Err(crate::wayland::hyprland_input::unknown_dispatch(
                    error.into(),
                    0,
                )),
                "clear",
            ),
        };
        drop(guard);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_an_overwritten_selection_never_claims_to_clear_the_new_owner() {
        let result = selection_result(Ok(json!({"ok":true,"cleared":false})), "clear");
        let body = result.structured_content.unwrap();
        assert_eq!(body["cleared"], false);
        assert_eq!(body["selection_scope"], "agent_seat");
        let record = result.action_record.unwrap();
        assert_eq!(record.actual_delivery, Some(ActualDelivery::NotApplicable));
        assert_eq!(
            record.transport.route(),
            cua_driver_core::action_record::ActionRoute::DataTransfer
        );
    }

    #[test]
    fn lost_reply_after_an_acknowledged_selection_phase_preserves_unknown_delivery() {
        let error = crate::wayland::hyprland_input::unknown_dispatch(
            anyhow::anyhow!("selection reply disconnected"),
            1,
        );
        let result = selection_result(Err(error), "set");
        let body = result.structured_content.unwrap();
        assert_eq!(body["effect"], "partial");
        assert_eq!(body["delivery"]["mode"], "unknown");
        assert_eq!(body["application_result"], "unverified");
        assert_eq!(result.action_record.unwrap().delivered_count, Some(1));
    }
}
