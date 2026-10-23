//! Terminal content and PTY input handlers for local-control actions.
use std::collections::HashMap;
use std::sync::Arc;

use ::local_control::protocol::{
    PaneInputState, PaneReadParams, PaneStreamMessage, PaneUpdateInputParams, TargetSelector,
    TextParams,
};
use ::local_control::{ActionKind, ControlError, ErrorCode, InstanceId};
use base64::Engine as _;
use futures::stream::{self, BoxStream, StreamExt as _};
use serde_json::json;
use string_offset::ByteOffset;
use warpui::{AppContext, EntityId, ModelContext, ViewHandle};

use crate::editor::Event as EditorEvent;
use crate::local_control::LocalControlBridge;
use crate::local_control::handlers::ack;
use crate::local_control::handlers::app_state::validate_staged_input_text;
use crate::local_control::resolver::{decode_params, input_target_pane_id, target_pane_group};
use crate::terminal::TerminalView;
use crate::terminal::model::block::{Block, BlockState};

const DEFAULT_MAX_BLOCKS: u32 = 20;
const MAX_BLOCKS_LIMIT: u32 = 500;
const INPUT_CHANNEL_CAPACITY: usize = 64;

/// Per-pane fan-out of input editor changes to `pane.stream` clients.
pub(crate) struct InputChannel {
    tx: async_broadcast::Sender<Arc<PaneStreamMessage>>,
    // Keeps the channel open while no stream is attached.
    _rx: async_broadcast::InactiveReceiver<Arc<PaneStreamMessage>>,
    applied_op_no: u64,
}

pub(crate) type InputChannels = HashMap<EntityId, InputChannel>;

pub(crate) fn handle(
    instance_id: &Option<InstanceId>,
    input_channels: &mut InputChannels,
    action: ActionKind,
    params: &serde_json::Value,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<serde_json::Value, ControlError> {
    match action {
        ActionKind::PaneRead => pane_read(action, params, target, ctx),
        ActionKind::PaneWrite => pane_write(instance_id, action, params, target, ctx),
        ActionKind::PaneRun => pane_run(instance_id, action, params, target, ctx),
        ActionKind::PaneUpdateInput => {
            pane_update_input(instance_id, input_channels, action, params, target, ctx)
        }
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
    let PaneReadParams { max_blocks, ansi } = decode_params(params)?;
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
            .map(|block| block_json(block, ansi))
            .collect::<Vec<_>>();
        let is_alt_screen_active = model.is_alt_screen_active();
        let alt_screen = is_alt_screen_active.then(|| {
            if ansi {
                model.alt_screen().output_to_ansi_string()
            } else {
                model.alt_screen().output_to_string()
            }
        });
        let size = model.block_list().size();
        json!({
            "action": action.as_str(),
            "alt_screen_active": is_alt_screen_active,
            "alt_screen": alt_screen,
            "blocks": blocks,
            "size": { "columns": size.columns(), "rows": size.rows() },
        })
    }))
}

fn block_json(block: &Block, ansi: bool) -> serde_json::Value {
    json!({
        "block_id": block.id().to_string(),
        "command": block.command_to_string(),
        "output": if ansi { block.output_to_ansi_string() } else { block.output_to_string() },
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

fn pane_update_input(
    instance_id: &Option<InstanceId>,
    input_channels: &mut InputChannels,
    action: ActionKind,
    params: &serde_json::Value,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<serde_json::Value, ControlError> {
    let PaneUpdateInputParams {
        op_no,
        text,
        cursor,
    } = decode_params(params)?;
    validate_staged_input_text(action, &text)?;
    let terminal_view = target_terminal_view(action, target, ctx)?;
    // Recorded first because the editor events this edit triggers publish the op number.
    if let Some(channel) = input_channels.get_mut(&terminal_view.id()) {
        channel.applied_op_no = op_no;
    }
    let cursor_byte = byte_offset_of_char(&text, cursor as usize);
    terminal_view.update(ctx, |terminal_view, ctx| {
        terminal_view.input().update(ctx, |input, ctx| {
            input.editor().update(ctx, |editor, ctx| {
                if editor.buffer_text(ctx) != text {
                    editor.set_buffer_text(&text, ctx);
                }
                editor.select_ranges_by_byte_offset(
                    [ByteOffset::from(cursor_byte).empty_range()],
                    ctx,
                );
            });
        });
    });
    Ok(ack(instance_id, action))
}

fn byte_offset_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn input_state(terminal_view: &TerminalView, op_no: u64, ctx: &AppContext) -> PaneInputState {
    let editor = terminal_view.input().as_ref(ctx).editor().as_ref(ctx);
    let text = editor.buffer_text(ctx);
    let cursor_byte = editor.start_byte_index_of_first_selection(ctx).as_usize();
    let cursor = text
        .get(..cursor_byte)
        .map_or(text.chars().count(), |before| before.chars().count());
    PaneInputState {
        buffer_id: terminal_view
            .model
            .lock()
            .block_list()
            .active_block_id()
            .to_string(),
        op_no,
        text,
        cursor: cursor as u32,
    }
}

fn input_receiver(
    input_channels: &mut InputChannels,
    terminal_view: &ViewHandle<TerminalView>,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> async_broadcast::Receiver<Arc<PaneStreamMessage>> {
    let view_id = terminal_view.id();
    if let Some(channel) = input_channels.get(&view_id) {
        return channel.tx.new_receiver();
    }
    let (mut tx, rx) = async_broadcast::broadcast(INPUT_CHANNEL_CAPACITY);
    tx.set_overflow(true);
    let receiver = rx.clone();
    input_channels.insert(
        view_id,
        InputChannel {
            tx,
            _rx: rx.deactivate(),
            applied_op_no: 0,
        },
    );
    let editor = terminal_view.read(ctx, |terminal_view, ctx| {
        terminal_view.input().as_ref(ctx).editor().clone()
    });
    let weak_view = terminal_view.downgrade();
    ctx.subscribe_to_view(&editor, move |bridge, _, event, ctx| {
        if !matches!(
            event,
            EditorEvent::Edited(_) | EditorEvent::BufferReplaced | EditorEvent::SelectionChanged
        ) {
            return;
        }
        let Some(terminal_view) = weak_view.upgrade(ctx) else {
            return;
        };
        let Some(channel) = bridge.input_channels().get(&view_id) else {
            return;
        };
        if channel.tx.receiver_count() == 0 {
            return;
        }
        let state = input_state(terminal_view.as_ref(ctx), channel.applied_op_no, ctx);
        let _ = channel
            .tx
            .try_broadcast(Arc::new(PaneStreamMessage::InputUpdated(state)));
    });
    receiver
}

/// The target pane's PTY output and input editor changes, starting with the current input.
pub(crate) fn pane_stream(
    input_channels: &mut InputChannels,
    action: ActionKind,
    target: &TargetSelector,
    ctx: &mut ModelContext<LocalControlBridge>,
) -> Result<BoxStream<'static, Arc<PaneStreamMessage>>, ControlError> {
    let terminal_view = target_terminal_view(action, target, ctx)?;
    let pty_reads = terminal_view
        .read(ctx, |terminal_view, ctx| {
            terminal_view.subscribe_pty_reads(ctx)
        })
        .ok_or_else(|| {
            ControlError::new(
                ErrorCode::UnsupportedAction,
                format!("{} requires a local terminal session", action.as_str()),
            )
        })?;
    let inputs = input_receiver(input_channels, &terminal_view, ctx);
    let applied_op_no = input_channels
        .get(&terminal_view.id())
        .map_or(0, |channel| channel.applied_op_no);
    let initial =
        PaneStreamMessage::InputUpdated(input_state(terminal_view.as_ref(ctx), applied_op_no, ctx));
    let pty_messages = pty_reads.map(|bytes| {
        Arc::new(PaneStreamMessage::PtyBytesRead {
            bytes: base64::engine::general_purpose::STANDARD.encode(bytes.as_slice()),
        })
    });
    Ok(stream::once(async move { Arc::new(initial) })
        .chain(stream::select(pty_messages, inputs))
        .boxed())
}
