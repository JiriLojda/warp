//! Terminal content and PTY input handlers for local-control actions.
use std::sync::Arc;

use ::local_control::protocol::{PaneReadParams, TargetSelector, TextParams};
use ::local_control::{ActionKind, ControlError, ErrorCode, InstanceId};
use serde_json::json;
use warpui::{ModelContext, ViewHandle};

use crate::local_control::LocalControlBridge;
use crate::local_control::handlers::ack;
use crate::local_control::handlers::app_state::validate_staged_input_text;
use crate::local_control::resolver::{decode_params, input_target_pane_id, target_pane_group};
use crate::terminal::TerminalView;
use crate::terminal::model::block::{Block, BlockState};

const DEFAULT_MAX_BLOCKS: u32 = 20;
const MAX_BLOCKS_LIMIT: u32 = 500;

pub(crate) fn handle(
    instance_id: &Option<InstanceId>,
    action: ActionKind,
    params: &serde_json::Value,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<serde_json::Value, ControlError> {
    match action {
        ActionKind::PaneRead => pane_read(action, params, target, ctx),
        ActionKind::PaneWrite => pane_write(instance_id, action, params, target, ctx),
        ActionKind::PaneRun => pane_run(instance_id, action, params, target, ctx),
        _ => Err(ControlError::new(
            ErrorCode::UnsupportedAction,
            format!("{} is not a terminal I/O action", action.as_str()),
        )),
    }
}

fn target_terminal_view(
    action: ActionKind,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<ViewHandle<TerminalView>, ControlError> {
    let pane_group = target_pane_group(action, target, ctx)?;
    let pane_id = input_target_pane_id(action, target, &pane_group, ctx)?;
    pane_group
        .read(ctx, |pane_group, ctx| {
            pane_group.terminal_view_from_pane_id(pane_id, ctx)
        })
        .ok_or_else(|| {
            ControlError::new(
                ErrorCode::MissingTarget,
                format!("{} requires a terminal pane target", action.as_str()),
            )
        })
}

fn pane_read(
    action: ActionKind,
    params: &serde_json::Value,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<serde_json::Value, ControlError> {
    let PaneReadParams { max_blocks } = decode_params(params)?;
    let max_blocks = max_blocks
        .unwrap_or(DEFAULT_MAX_BLOCKS)
        .min(MAX_BLOCKS_LIMIT) as usize;
    let terminal_view = target_terminal_view(action, target, ctx)?;
    Ok(terminal_view.read(ctx, |terminal_view, _| {
        let model = terminal_view.model.lock();
        let visible_blocks = model
            .block_list()
            .blocks()
            .iter()
            .filter(|block| !block.is_hidden())
            .collect::<Vec<_>>();
        let blocks = visible_blocks
            .iter()
            .skip(visible_blocks.len().saturating_sub(max_blocks))
            .map(|block| block_json(block))
            .collect::<Vec<_>>();
        let is_alt_screen_active = model.is_alt_screen_active();
        json!({
            "action": action.as_str(),
            "alt_screen_active": is_alt_screen_active,
            "alt_screen": is_alt_screen_active.then(|| model.alt_screen().output_to_string()),
            "blocks": blocks,
        })
    }))
}

fn block_json(block: &Block) -> serde_json::Value {
    json!({
        "block_id": block.id().to_string(),
        "command": block.command_to_string(),
        "output": block.output_to_string(),
        "state": block_state_name(block.state()),
        "exit_code": block.is_done().then(|| block.exit_code().value()),
        "pwd": block.pwd(),
    })
}

fn block_state_name(state: BlockState) -> &'static str {
    match state {
        BlockState::BeforeExecution => "before_execution",
        BlockState::Executing => "executing",
        BlockState::DoneWithExecution => "done",
        BlockState::DoneWithNoExecution => "done_without_execution",
        BlockState::Background => "background",
        BlockState::Static => "static",
    }
}

fn pane_write(
    instance_id: &Option<InstanceId>,
    action: ActionKind,
    params: &serde_json::Value,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<serde_json::Value, ControlError> {
    let TextParams { text } = decode_params(params)?;
    if text.is_empty() {
        return Err(empty_text_error(action));
    }
    let terminal_view = target_terminal_view(action, target, ctx)?;
    // At an idle prompt the bytes would land in the shell's hidden line editor and get prepended to the next command.
    if !is_command_running(&terminal_view, ctx) {
        return Err(ControlError::new(
            ErrorCode::TargetStateConflict,
            format!(
                "{} requires a running command; use pane.run or input.insert at an idle prompt",
                action.as_str()
            ),
        ));
    }
    let forwarded = terminal_view.update(ctx, |terminal_view, ctx| {
        terminal_view.write_user_bytes_to_pty(text.into_bytes(), ctx)
    });
    if !forwarded {
        return Err(ControlError::new(
            ErrorCode::TargetStateConflict,
            format!(
                "{} was rejected because an agent controls the active block",
                action.as_str()
            ),
        ));
    }
    Ok(ack(instance_id, action))
}

fn pane_run(
    instance_id: &Option<InstanceId>,
    action: ActionKind,
    params: &serde_json::Value,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<serde_json::Value, ControlError> {
    let TextParams { text } = decode_params(params)?;
    if text.trim().is_empty() {
        return Err(empty_text_error(action));
    }
    validate_staged_input_text(action, &text)?;
    let terminal_view = target_terminal_view(action, target, ctx)?;
    // Commands submitted while another runs are dropped by the input editor, so refuse instead of queueing.
    let has_pending_command = terminal_view.read(ctx, |terminal_view, ctx| {
        terminal_view.input().as_ref(ctx).has_pending_command()
    });
    if has_pending_command || is_command_running(&terminal_view, ctx) {
        return Err(ControlError::new(
            ErrorCode::TargetStateConflict,
            format!(
                "{} requires an idle prompt; wait for the running command or use pane.write",
                action.as_str()
            ),
        ));
    }
    let queued = terminal_view.update(ctx, |terminal_view, ctx| {
        // Pending commands are inserted at the cursor, so leftover editor text would be submitted with them.
        terminal_view.input().update(ctx, |input, ctx| {
            input.replace_buffer_content("", ctx);
        });
        terminal_view.execute_command_or_set_pending(&text, ctx);
        terminal_view.input().as_ref(ctx).has_pending_command()
    });
    let mut response = ack(instance_id, action);
    response["queued"] = json!(queued);
    Ok(response)
}

fn is_command_running(
    terminal_view: &ViewHandle<TerminalView>,
    ctx: &ModelContext<LocalControlBridge>,
) -> bool {
    terminal_view.read(ctx, |terminal_view, _| {
        let model = terminal_view.model.lock();
        model.is_alt_screen_active()
            || model.block_list().active_block().state() == BlockState::Executing
    })
}

fn empty_text_error(action: ActionKind) -> ControlError {
    ControlError::new(
        ErrorCode::InvalidParams,
        format!("{} requires non-empty text", action.as_str()),
    )
}

pub(crate) fn pane_stream(
    action: ActionKind,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<async_broadcast::Receiver<Arc<Vec<u8>>>, ControlError> {
    let terminal_view = target_terminal_view(action, target, ctx)?;
    terminal_view
        .read(ctx, |terminal_view, ctx| {
            terminal_view.subscribe_pty_reads(ctx)
        })
        .ok_or_else(|| {
            ControlError::new(
                ErrorCode::UnsupportedAction,
                format!("{} requires a local terminal session", action.as_str()),
            )
        })
}
