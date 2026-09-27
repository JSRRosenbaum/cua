use super::*;
use cua_driver_core::action_record::{
    ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
};
use cua_driver_core::tool_args::ArgsExt;

pub struct TransferDataTool {
    state: Arc<ToolState>,
}

impl TransferDataTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static TRANSFER_DEF: std::sync::LazyLock<ToolDef> = std::sync::LazyLock::new(|| {
    ToolDef {
    name: "transfer_data".into(),
    description: "Transfer data between independently captured source and destination windows. Background requires native Wayland on both sides and a real agent-seat wl_data_device source/offer/accept/receive/drop/finish; pointer motion is not proof. Explicit delivery_mode:\"foreground\" uses the physical compositor pointer only when both captured windows have compositor-attested surface proofs (including XWayland XID/root identity) and physical-input interlocks permit an authorized takeover. Foreground pointer delivery does not prove an XWM data offer, an app-owned result, or private-seat data-device finish. Does not mutate the desktop clipboard.".into(),
    input_schema: json!({"type":"object","required":[
        "session", "source_pid", "source_window_id", "source_capture_id", "source_x", "source_y",
        "destination_pid", "destination_window_id", "destination_capture_id", "destination_x", "destination_y"
    ],"properties":{
        "session":cua_driver_core::tool_schema::session_schema(),
        "source_pid":{"type":"integer","minimum":1},
        "source_window_id":{"type":"integer","minimum":1},
        "source_capture_id":{"type":"string","minLength":1},
        "source_x":{"type":"number","description":"Source window-local X in source_capture_id PNG pixels."},
        "source_y":{"type":"number","description":"Source window-local Y in source_capture_id PNG pixels."},
        "destination_pid":{"type":"integer","minimum":1},
        "destination_window_id":{"type":"integer","minimum":1},
        "destination_capture_id":{"type":"string","minLength":1},
        "destination_x":{"type":"number","description":"Destination window-local X in destination_capture_id PNG pixels."},
        "destination_y":{"type":"number","description":"Destination window-local Y in destination_capture_id PNG pixels."},
        "duration_ms":{"type":"integer","minimum":100,"maximum":2000,"default":600},
        "delivery_mode":{"type":"string","enum":["background","foreground"],"default":"background","description":"foreground explicitly uses primary compositor input; background uses the isolated native agent seat."}
    },"additionalProperties":false}),
    read_only: false, destructive: true, idempotent: false, open_world: true,
}
});

fn record_with_route(
    effect: ActionEffect,
    delivery: Option<ActualDelivery>,
    phases: u32,
    detail: &str,
    foreground: bool,
) -> ActionExecutionRecord {
    let mut builder = ActionExecutionRecord::builder(
        effect,
        if foreground {
            ActionTransport::LinuxHyprlandForegroundInput
        } else {
            ActionTransport::LinuxHyprlandDataTransfer
        },
        if foreground {
            RequestedDelivery::Foreground
        } else {
            RequestedDelivery::Background
        },
    )
    .detail(detail);
    if let Some(delivery) = delivery {
        builder = builder.actual_delivery(delivery);
    }
    if phases > 0 {
        builder = builder.delivered_count(phases);
    }
    builder.build().expect("data transfer record is valid")
}
fn record(
    effect: ActionEffect,
    delivery: Option<ActualDelivery>,
    phases: u32,
    detail: &str,
) -> ActionExecutionRecord {
    record_with_route(effect, delivery, phases, detail, false)
}

fn refusal(code: &str, detail: impl Into<String>) -> ToolResult {
    let detail = detail.into();
    ToolResult::error(format!("{code}: {detail}"))
        .with_structured(json!({"code":code,"detail":detail,"effect":"refused",
            "route":"not_attempted","application_result":"unverified"}))
        .with_action_record(record(ActionEffect::Refused, None, 0, &detail))
}

fn transfer_result(result: anyhow::Result<Value>) -> ToolResult {
    match result {
        Ok(value) => {
            let fields = [
                "source_started",
                "offered",
                "accepted",
                "receive_requested",
                "drop_performed",
                "finished",
            ];
            let phases = fields.iter().filter(|field| value[*field] == true).count() as u32
                + u32::from(value["press_delivered"] == true);
            let unknown = value["effect"] == "unknown"
                || value["delivery"]["mode"] == "unknown"
                || (value["effect"] == "partial" && phases == 0);
            let protocol_finished = value["ok"] == true
                && !unknown
                && fields.iter().all(|field| value[field] == true)
                && value["accepted_mime"]
                    .as_str()
                    .is_some_and(|mime| !mime.is_empty())
                && (value["chosen_action"]
                    .as_u64()
                    .is_some_and(|action| action != 0)
                    || value["chosen_action"]
                        .as_str()
                        .is_some_and(|action| !action.is_empty() && action != "none"));
            let explicit_refusal = phases == 0
                && !unknown
                && (value["effect"] == "refused"
                    || (value["ok"] == false && value["effect"] != "partial"));
            let effect = if explicit_refusal {
                ActionEffect::Refused
            } else if unknown && phases == 0 {
                ActionEffect::Unverifiable
            } else if protocol_finished {
                ActionEffect::Unverifiable
            } else if phases > 0 {
                ActionEffect::Partial
            } else {
                ActionEffect::Unverifiable
            };
            let delivery = match effect {
                ActionEffect::Refused => None,
                _ if unknown => Some(ActualDelivery::Unknown),
                _ => Some(ActualDelivery::Background),
            };
            let code = value["code"].as_str().unwrap_or(if protocol_finished {
                "transfer_protocol_finished"
            } else if explicit_refusal {
                "transfer_refused"
            } else {
                "transfer_application_unverified"
            });
            let detail = value["detail"].as_str().unwrap_or(
                "Wayland data transfer outcome recorded; destination application result unverified",
            );
            let mut structured = json!({
                "route":"seat_data_device", "effect":match effect {
                    ActionEffect::Refused => "refused", ActionEffect::Partial => "partial", _ => "unverifiable"
                }, "code":code, "detail":detail,
                "press_delivered":value["press_delivered"], "source_started":value["source_started"], "offered":value["offered"],
                "accepted":value["accepted"], "receive_requested":value["receive_requested"],
                "drop_performed":value["drop_performed"], "finished":value["finished"],
                "chosen_action":value["chosen_action"], "accepted_mime":value["accepted_mime"],
                "protocol_finished":protocol_finished, "application_result":"unverified", "verified":false
            });
            if unknown {
                structured["delivery"] = json!({"mode":"unknown"});
            }
            let result = if explicit_refusal {
                ToolResult::error(format!("Transfer refused before source drag: {detail}"))
            } else if protocol_finished {
                ToolResult::text("Wayland data offer was accepted, received, dropped, and finished; inspect the destination application's result.")
            } else {
                ToolResult::error(format!(
                    "Wayland data transfer did not finish; inspect before retrying: {detail}"
                ))
            };
            result
                .with_structured(structured)
                .with_action_record(record(effect, delivery, phases, detail))
        }
        Err(error) => {
            let acknowledged = error
                .downcast_ref::<crate::wayland::hyprland_input::DispatchUnknown>()
                .map(|unknown| unknown.acknowledged_phases)
                .unwrap_or(0);
            let effect = if acknowledged > 0 {
                ActionEffect::Partial
            } else {
                ActionEffect::Unverifiable
            };
            ToolResult::error(format!("Data transfer delivery is unknown; inspect both applications before retrying: {error}"))
                .with_structured(json!({"code":"transfer_delivery_unknown", "effect":if acknowledged > 0 {"partial"} else {"unverifiable"},
                    "route":"seat_data_device", "delivery":{"mode":"unknown","acknowledged_phases":acknowledged},
                    "application_result":"unverified"}))
                .with_action_record(record(effect, Some(ActualDelivery::Unknown), acknowledged, &error.to_string()))
        }
    }
}

fn foreground_result(result: anyhow::Result<Value>) -> ToolResult {
    match result {
        Ok(value) => {
            let press = value["press_delivered"] == true;
            let motion = value["motion_delivered"] == true;
            let release = value["release_delivered"] == true;
            let phases = u32::from(press) + u32::from(motion) + u32::from(release);
            let unknown = value["effect"] == "unknown"
                || value["delivery"]["mode"] == "unknown"
                || (value["effect"] == "partial" && phases == 0);
            let refused = phases == 0
                && !unknown
                && (value["effect"] == "refused"
                    || (value["ok"] == false && value["effect"] != "partial"));
            let complete = value["ok"] == true && !unknown && press && motion && release;
            let effect = if refused {
                ActionEffect::Refused
            } else if complete || phases == 0 {
                ActionEffect::Unverifiable
            } else {
                ActionEffect::Partial
            };
            let detail = value["detail"].as_str().unwrap_or(
                "primary compositor pointer delivery; application data transfer unverified",
            );
            let code = value["code"].as_str().unwrap_or(if refused {
                "foreground_transfer_refused"
            } else {
                "foreground_delivery_unverified"
            });
            let result = if refused {
                ToolResult::error(format!("Foreground drag refused before press: {detail}"))
            } else if complete {
                ToolResult::text("Foreground drag input delivered to the compositor; inspect the destination application's data-transfer result.")
            } else {
                ToolResult::error(format!("Foreground drag incomplete or unknown; inspect both applications before retrying: {detail}"))
            };
            result.with_structured(json!({
                "route":"primary_foreground", "delivery_mode":"foreground",
                "effect":if refused {"refused"} else if phases > 0 && !complete {"partial"} else {"unverifiable"},
                "code":code, "detail":detail, "press_delivered":value["press_delivered"],
                "motion_delivered":value["motion_delivered"], "release_delivered":value["release_delivered"],
                "input_delivered":complete, "data_offer_result":"unverified",
                "application_result":"unverified", "verified":false
            })).with_action_record(record_with_route(
                effect,
                if refused { None } else if unknown { Some(ActualDelivery::Unknown) }
                    else if phases > 0 { Some(ActualDelivery::Foreground) }
                    else { Some(ActualDelivery::Unknown) },
                phases, detail, true,
            ))
        }
        Err(error) => {
            let acknowledged = error
                .downcast_ref::<crate::wayland::hyprland_input::DispatchUnknown>()
                .map(|unknown| unknown.acknowledged_phases)
                .unwrap_or(0);
            let effect = if acknowledged > 0 {
                ActionEffect::Partial
            } else {
                ActionEffect::Unverifiable
            };
            ToolResult::error(format!("Foreground drag outcome unknown; inspect both applications before retrying: {error}"))
                .with_structured(json!({
                    "route":"primary_foreground", "delivery_mode":"foreground",
                    "code":"foreground_transfer_unknown", "effect":if acknowledged > 0 {"partial"} else {"unverifiable"},
                    "delivery":{"mode":"unknown","acknowledged_phases":acknowledged},
                    "data_offer_result":"unverified", "application_result":"unverified"
                }))
                .with_action_record(record_with_route(effect, Some(ActualDelivery::Unknown), acknowledged, &error.to_string(), true))
        }
    }
}

fn required_target<'a>(
    args: &'a Value,
    prefix: &str,
) -> Result<(u32, u64, &'a str, f64, f64), ToolResult> {
    let (pid_key, address_key, capture_key, x_key, y_key) = match prefix {
        "source" => (
            "source_pid",
            "source_window_id",
            "source_capture_id",
            "source_x",
            "source_y",
        ),
        "destination" => (
            "destination_pid",
            "destination_window_id",
            "destination_capture_id",
            "destination_x",
            "destination_y",
        ),
        _ => unreachable!("only source and destination transfer endpoints exist"),
    };
    let pid = args
        .opt_u64(pid_key)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n != 0)
        .ok_or_else(|| {
            refusal(
                "transfer_target_required",
                format!("{pid_key} must name an exact process"),
            )
        })?;
    let address = args
        .opt_u64(address_key)
        .filter(|n| *n != 0)
        .ok_or_else(|| {
            refusal(
                "transfer_target_required",
                format!("{address_key} must name an exact window"),
            )
        })?;
    let capture = args
        .get(capture_key)
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            refusal(
                "transfer_capture_required",
                format!("{capture_key} is required"),
            )
        })?;
    let x = args
        .get(x_key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| refusal("transfer_point_required", format!("{x_key} must be finite")))?;
    let y = args
        .get(y_key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| refusal("transfer_point_required", format!("{y_key} must be finite")))?;
    Ok((pid, address, capture, x, y))
}

#[async_trait]
impl Tool for TransferDataTool {
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        // Only the two exact native windows on the isolated agent seat can
        // bypass shared physical-input coordination. Every foreground,
        // XWayland, ambiguous, or malformed request retains the coordinator.
        if !matches!(
            args.get("delivery_mode").and_then(Value::as_str),
            None | Some("background")
        ) || !crate::wayland::hyprland_input::enabled()
            || !crate::wayland::hyprland::is_session()
            || !["source_capture_id", "destination_capture_id"]
                .iter()
                .all(|key| {
                    args.get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
                })
        {
            return false;
        }
        let target = |pid_key: &str, address_key: &str| {
            let pid = args
                .get(pid_key)
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0)?;
            let address = args
                .get(address_key)
                .and_then(Value::as_u64)
                .filter(|address| *address != 0)?;
            Some((pid, address))
        };
        let (Some(source), Some(destination)) = (
            target("source_pid", "source_window_id"),
            target("destination_pid", "destination_window_id"),
        ) else {
            return false;
        };
        source != destination
            && matches!(
                crate::wayland::hyprland::target_kind(source.0, source.1),
                Ok(crate::wayland::hyprland::TargetKind::Native)
            )
            && matches!(
                crate::wayland::hyprland::target_kind(destination.0, destination.1),
                Ok(crate::wayland::hyprland::TargetKind::Native)
            )
    }

    fn def(&self) -> &ToolDef {
        &TRANSFER_DEF
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(owner) = named_session_cursor_key(&args) else {
            return refusal(
                "transfer_lifecycle_required",
                "an authenticated session is required",
            );
        };
        let foreground = match args
            .get("delivery_mode")
            .and_then(Value::as_str)
            .unwrap_or("background")
        {
            "background" => false,
            "foreground" => true,
            _ => {
                return refusal(
                    "transfer_invalid_delivery_mode",
                    "delivery_mode must be background or foreground",
                )
            }
        };
        let (source_pid, source_address, source_capture, sx, sy) =
            match required_target(&args, "source") {
                Ok(target) => target,
                Err(refused) => return refused,
            };
        let (dest_pid, dest_address, dest_capture, dx, dy) =
            match required_target(&args, "destination") {
                Ok(target) => target,
                Err(refused) => return refused,
            };
        if source_address == dest_address || source_capture == dest_capture {
            return refusal(
                "transfer_distinct_windows_required",
                "source and destination must be independently observed, different windows",
            );
        }
        let duration_ms = args.opt_u64("duration_ms").unwrap_or(600);
        if !(100..=2000).contains(&duration_ms) {
            return refusal(
                "transfer_invalid_duration",
                "duration_ms must be 100..=2000",
            );
        }
        if !crate::wayland::hyprland_input::enabled() {
            return refusal(
                "transfer_route_unavailable",
                "Hyprland transfer protocol is unavailable",
            );
        }
        if !foreground {
            for (pid, address, label) in [
                (source_pid, source_address, "source"),
                (dest_pid, dest_address, "destination"),
            ] {
                match crate::wayland::hyprland::target_kind(pid, address) {
                    Ok(crate::wayland::hyprland::TargetKind::Native) => {}
                    Ok(crate::wayland::hyprland::TargetKind::XWayland) => {
                        return refusal("transfer_native_target_required", format!("{label} is XWayland; a native seat cannot authenticate its data source"));
                    }
                    Err(error) => {
                        return refusal("transfer_target_unavailable", format!("{label}: {error}"))
                    }
                }
            }
        }
        // Each one-shot PNG carries its own process/window/session and compositor
        // layout identity. Both source and destination must pass before a press.
        let source_admission = match crate::capture_action_frame::admit_window_click(
            &self.state.capture_service,
            &args,
            source_capture,
            source_pid,
            source_address,
            sx,
            sy,
        ) {
            Ok(point) => point,
            Err(error) => {
                return refusal(
                    cua_driver_core::capture_runtime::admission_error_code(&error),
                    format!("source capture was not admissible: {error}"),
                )
            }
        };
        let dest_admission = match crate::capture_action_frame::admit_window_click(
            &self.state.capture_service,
            &args,
            dest_capture,
            dest_pid,
            dest_address,
            dx,
            dy,
        ) {
            Ok(point) => point,
            Err(error) => {
                return refusal(
                    cua_driver_core::capture_runtime::admission_error_code(&error),
                    format!("destination capture was not admissible: {error}"),
                )
            }
        };
        let Some(source_proof) = source_admission.proof else {
            return refusal(
                "transfer_source_proof_unavailable",
                "the source capture lacks a compositor-attested surface proof",
            );
        };
        let Some(dest_proof) = dest_admission.proof else {
            return refusal(
                "transfer_destination_proof_unavailable",
                "the destination capture lacks a compositor-attested surface proof",
            );
        };
        let source_local = source_admission.point;
        let dest_local = dest_admission.point;
        let (guard, dispatch) = match spawn_isolated_hyprland(&args, move |cancellation| {
            if foreground {
                crate::wayland::hyprland_input::execute_foreground_transfer(
                    Some(owner),
                    source_pid,
                    source_address,
                    dest_pid,
                    dest_address,
                    source_local,
                    dest_local,
                    source_proof,
                    dest_proof,
                    duration_ms,
                    cancellation,
                )
            } else {
                crate::wayland::hyprland_input::execute_transfer(
                    Some(owner),
                    source_pid,
                    source_address,
                    dest_pid,
                    dest_address,
                    source_local,
                    dest_local,
                    source_proof,
                    dest_proof,
                    duration_ms,
                    cancellation,
                )
            }
        }) {
            Ok(dispatch) => dispatch,
            Err(result) => return result,
        };
        let result = match dispatch.await {
            Ok(result) => {
                if foreground {
                    foreground_result(result)
                } else {
                    transfer_result(result)
                }
            }
            Err(error) => {
                let unknown = Err(crate::wayland::hyprland_input::unknown_dispatch(
                    error.into(),
                    0,
                ));
                if foreground {
                    foreground_result(unknown)
                } else {
                    transfer_result(unknown)
                }
            }
        };
        drop(guard);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_without_receipt_or_finish_is_not_reported_as_transfer() {
        let result = transfer_result(Ok(json!({"ok":true,"source_started":true,"offered":true,
            "accepted":true,"receive_requested":false,"drop_performed":true,"finished":false,
            "chosen_action":"copy","accepted_mime":"text/plain"})));
        let data = result.structured_content.unwrap();
        assert_eq!(data["protocol_finished"], false);
        assert_eq!(data["effect"], "partial");
        assert_eq!(data["application_result"], "unverified");
    }

    #[test]
    fn protocol_finish_remains_distinct_from_application_result() {
        let result = transfer_result(Ok(json!({"ok":true,"source_started":true,"offered":true,
            "accepted":true,"receive_requested":true,"drop_performed":true,"finished":true,
            "chosen_action":"copy","accepted_mime":"text/plain"})));
        let data = result.structured_content.unwrap();
        assert_eq!(data["protocol_finished"], true);
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["application_result"], "unverified");
    }

    #[test]
    fn source_press_with_lost_reply_is_not_replayed() {
        let result = transfer_result(Err(crate::wayland::hyprland_input::unknown_dispatch(
            anyhow::anyhow!("reply lost"),
            1,
        )));
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "partial");
        assert_eq!(data["delivery"]["mode"], "unknown");
    }

    #[test]
    fn pre_press_target_rejection_is_refused_not_delivered() {
        let result = transfer_result(Ok(json!({"ok":false,"code":"data_device_unavailable",
            "detail":"source has no bound data device"})));
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["protocol_finished"], false);
    }

    #[test]
    fn delivered_press_without_data_source_is_partial_not_a_transfer() {
        let result = transfer_result(Ok(json!({"ok":false,"effect":"partial",
            "code":"data_source_not_started","press_delivered":true,
            "source_started":false,"offered":false,"accepted":false,
            "receive_requested":false,"drop_performed":false,"finished":false})));
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "partial");
        assert_eq!(data["protocol_finished"], false);
        assert_eq!(data["press_delivered"], true);
    }

    #[test]
    fn foreground_pointer_release_does_not_imply_xdnd_offer_or_app_effect() {
        let result = foreground_result(Ok(json!({"ok":true,"effect":"unverifiable",
            "press_delivered":true,"motion_delivered":true,"release_delivered":true})));
        let data = result.structured_content.unwrap();
        assert_eq!(data["input_delivered"], true);
        assert_eq!(data["data_offer_result"], "unverified");
        assert_eq!(data["application_result"], "unverified");
    }
}
