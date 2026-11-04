//! The Claude chat: a floating button in the window's bottom-right corner that opens into a pill
//! to type in, with the conversation in a rounded square above it (bottom-aligned). Claude
//! answers through Claude Code ([`bridge`]) and changes the document only by proposing edits
//! (`ai.propose`), which the user applies (`ai.apply`, one undo step) or discards.
//!
//! Motion follows the Animation effects setting: with it off, opening, closing and the change
//! highlight are instant.

pub mod bridge;

use std::sync::mpsc::Receiver;

use egui::{Align, Align2, Color32, CornerRadius, Key, Layout, Modifiers, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};
use serde_json::json;
use wordcraft_engine::doc::{Pos, StoryRef};

use crate::i18n::t;
use crate::theme::Tokens;
use crate::{ControlRequest, WordApp};

/// The closed button's diameter, and the pill's height for one line.
const BUTTON: f32 = 48.0;
/// Gap to the window's edges; the bottom also clears the status bar.
const MARGIN: f32 = 16.0;
const STATUS_BAR: f32 = 28.0;
/// Widest the pill and the square get.
const WIDTH: f32 = 380.0;
/// Most lines the pill grows to before it scrolls.
const MAX_LINES: f32 = 8.0;
/// Open/close time and the change highlight's reveal and glow, in seconds.
const OPEN_SECS: f32 = 0.3;
const REVEAL_SECS: f64 = 0.6;
const GLOW_SECS: f64 = 1.6;
/// Characters of the selection shown in the reference chip.
const CHIP_CHARS: usize = 40;

/// One message of the conversation.
#[derive(Clone, Debug, Default)]
pub struct Message {
    pub from_user: bool,
    pub text: String,
}

/// The selected range a message refers to.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    pub anchor: Pos,
    pub focus: Pos,
    pub text: String,
}

/// An applied edit fading in.
#[derive(Clone, Debug)]
struct Anim {
    start: Pos,
    end: Pos,
    since: f64,
}

/// Starts the control channel on demand, for an app launched without `--control`: the receiver
/// and the port it listens on.
pub type Connect = Box<dyn FnMut(&egui::Context) -> Option<(Receiver<ControlRequest>, u16)>>;

#[derive(Default)]
pub struct ChatState {
    pub open: bool,
    pub input: String,
    pub messages: Vec<Message>,
    /// What Claude is doing right now (a tool), for the status line.
    pub status: Option<String>,
    pub reference: Option<Reference>,
    /// A reference the user removed: not offered again until the selection changes.
    dismissed: Option<(Pos, Pos)>,
    #[cfg(not(target_arch = "wasm32"))]
    reply: Option<bridge::Reply>,
    session: Option<String>,
    anims: Vec<Anim>,
    seen_applied: u64,
    /// The conversation's height last frame, to bottom-align a short one.
    content_h: f32,
    /// The panel's rectangle last frame, for clicks outside it.
    last_rect: Option<Rect>,
    focus_input: bool,
    /// The control port Claude's MCP bridge connects to.
    pub port: Option<u16>,
    pub connect: Option<Connect>,
}

impl ChatState {
    pub fn busy(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return self.reply.is_some();
        #[cfg(target_arch = "wasm32")]
        false
    }

    /// Open or close; opening puts the keyboard in the pill.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.focus_input = self.open;
    }

    /// Forget the conversation (a new Claude Code session next time).
    pub fn new_chat(&mut self) {
        self.messages.clear();
        self.session = None;
        self.status = None;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.reply = None;
        }
    }
}

fn now(ctx: &egui::Context) -> f64 {
    ctx.input(|i| i.time)
}

/// The document outline for Claude: headings and the paragraph count.
fn outline(app: &WordApp) -> String {
    let doc = &app.session.doc;
    let paths = doc.para_paths(StoryRef::Body);
    let mut out = String::new();
    for (i, path) in paths.iter().enumerate() {
        let Some(p) = doc.para(StoryRef::Body, path) else { continue };
        if p.props.style.as_deref().is_some_and(|s| s.to_ascii_lowercase().starts_with("heading") || s == "Title") {
            let text: String = p.plain_text().chars().take(120).collect();
            out.push_str(&format!("- [{i}] {text}\n"));
        }
        if out.len() > 8_000 {
            break;
        }
    }
    if out.is_empty() {
        out.push_str("(keine Überschriften)\n");
    }
    out.push_str(&format!("{} Absätze insgesamt.", paths.len()));
    out
}

/// The control port, starting the channel if the app has none yet.
fn ensure_port(app: &mut WordApp, ctx: &egui::Context) -> Option<u16> {
    if app.chat.port.is_none()
        && let Some(connect) = app.chat.connect.as_mut()
        && let Some((rx, port)) = connect(ctx)
    {
        if app.control_rx.is_none() {
            app.control_rx = Some(rx);
        }
        app.chat.port = Some(port);
    }
    app.chat.port
}

/// Send what is in the pill (Enter, the send button, `ui.claude.send`).
pub fn send(app: &mut WordApp, ctx: &egui::Context) {
    let text = app.chat.input.trim().to_string();
    if text.is_empty() || app.chat.busy() {
        return;
    }
    app.chat.input.clear();
    app.chat.messages.push(Message { from_user: true, text: text.clone() });
    app.chat.messages.push(Message { from_user: false, text: String::new() });
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(port) = ensure_port(app, ctx) else {
            if let Some(m) = app.chat.messages.last_mut() {
                m.text = t("The control channel couldn't start, so Claude can't reach the document.").to_string();
            }
            return;
        };
        let selection = app.chat.reference.as_ref().map(|r| r.text.clone());
        let system = bridge::system_prompt(&outline(app), selection.as_deref());
        app.chat.status = Some(t("Thinking…").to_string());
        app.chat.reply = Some(bridge::start(&text, &system, app.chat.session.as_deref(), port, ctx.clone()));
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ctx;
        if let Some(m) = app.chat.messages.last_mut() {
            m.text = t("The Claude chat needs the desktop app.").to_string();
        }
    }
}

/// The conversation for scripts (`ui.claude.state`).
pub fn state(app: &WordApp) -> serde_json::Value {
    let msgs: Vec<serde_json::Value> = app.chat.messages.iter().map(|m| json!({"user": m.from_user, "text": m.text})).collect();
    json!({"open": app.chat.open, "busy": app.chat.busy(), "status": app.chat.status, "messages": msgs, "proposal": app.session.doc_ai.proposal.is_some()})
}

/// Take what the running reply sent since the last frame.
fn poll(app: &mut WordApp) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(reply) = app.chat.reply.as_ref() else { return };
        let mut end = false;
        while let Ok(ev) = reply.rx.try_recv() {
            match ev {
                bridge::ChatEvent::Session(id) => app.chat.session = Some(id),
                bridge::ChatEvent::Delta(d) => {
                    if let Some(m) = app.chat.messages.last_mut() {
                        m.text.push_str(&d);
                    }
                    app.chat.status = None;
                }
                bridge::ChatEvent::Tool(name) => {
                    app.chat.status = Some(match name.as_str() {
                        "read_document" => t("Reading the document…").to_string(),
                        "propose_edits" => t("Preparing a suggestion…").to_string(),
                        other => other.to_string(),
                    });
                }
                bridge::ChatEvent::Done => end = true,
                bridge::ChatEvent::Error(e) => {
                    if let Some(m) = app.chat.messages.last_mut() {
                        if !m.text.is_empty() {
                            m.text.push_str("\n\n");
                        }
                        m.text.push_str(&e);
                    }
                    end = true;
                }
            }
        }
        if end {
            app.chat.reply = None;
            app.chat.status = None;
        }
    }
}

/// Follow the selection while the panel is open: a new selection becomes the reference.
fn track_selection(app: &mut WordApp) {
    let sel = &app.session.sel;
    if sel.is_collapsed() {
        return;
    }
    let key = (sel.anchor.clone(), sel.focus.clone());
    if app.chat.dismissed.as_ref() == Some(&key) {
        return;
    }
    app.chat.dismissed = None;
    if app.chat.reference.as_ref().is_some_and(|r| r.anchor == key.0 && r.focus == key.1) {
        return;
    }
    let text = wordcraft_engine::cmd::ai::selected_text(&app.session);
    app.chat.reference = Some(Reference { anchor: key.0, focus: key.1, text });
}

/// Run an assistant command from a button, keeping the panel's message on failure.
fn run_ai(app: &mut WordApp, id: &str) {
    if let Err(e) = app.run(id, json!({})) {
        app.chat.messages.push(Message { from_user: false, text: e });
    }
}

/// The panel, drawn over the document each frame.
pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    poll(app);
    let motion = app.animations_on();
    // New applied edits start their highlight.
    if app.session.doc_ai.applied != app.chat.seen_applied {
        app.chat.seen_applied = app.session.doc_ai.applied;
        let since = now(ctx);
        let marks = std::mem::take(&mut app.session.doc_ai.marks);
        app.chat.anims = marks.into_iter().map(|m| Anim { start: m.start, end: m.end, since }).collect();
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::ALT, Key::K)) {
        app.chat.toggle();
    }
    if app.chat.open {
        track_selection(app);
    }
    // File › (Backstage) covers the document; the chat belongs to the document.
    if app.ui.backstage || !app.has_document() {
        return;
    }
    // An open context menu owns outside clicks; the closed circle must not catch them.
    if !app.chat.open && app.canvas.context_menu_open {
        return;
    }
    let t_open = ctx.animate_bool_with_time_and_easing(
        egui::Id::unique("claude_chat_open"),
        app.chat.open,
        if motion { OPEN_SECS } else { 0.0 },
        egui::emath::easing::cubic_out,
    );
    let tok = Tokens::get(ctx);
    let area = ctx.content_rect();
    let width = (area.width() - 2.0 * MARGIN).clamp(BUTTON, WIDTH);
    let input_id = egui::Id::unique("claude_chat_input");
    let mut rect = None;
    egui::Area::new(egui::Id::unique("claude_chat"))
        .order(egui::Order::Foreground)
        .anchor(Align2::RIGHT_BOTTOM, vec2(-MARGIN, -(MARGIN + STATUS_BAR)))
        .show(ctx, |ui| {
            ui.set_max_width(width);
            if t_open <= 0.0 {
                closed_button(app, ui, &tok);
            } else if t_open < 1.0 {
                // Growing from the circle into the pill.
                let w = egui::lerp(BUTTON..=width, t_open);
                let (r, _) = ui.allocate_exact_size(vec2(w, BUTTON), Sense::hover());
                ui.painter().rect(r, CornerRadius::same(24), tok.panel, Stroke::new(1.0, tok.border), egui::StrokeKind::Inside);
                spark(ui.painter(), pos2(r.max.x - BUTTON / 2.0, r.center().y), 10.0, tok.ai_accent.gamma_multiply(1.0 - t_open));
                ctx.request_repaint();
            } else {
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    if !app.chat.messages.is_empty() || app.session.doc_ai.proposal.is_some() {
                        conversation(app, ui, &tok, width, area);
                        ui.add_space(8.0);
                    }
                    pill(app, ui, ctx, &tok, width, input_id);
                });
            }
            rect = Some(ui.min_rect());
        });
    app.chat.last_rect = rect;
    if app.chat.open && t_open >= 1.0 {
        close_keys(app, ctx, input_id);
    }
    if app.chat.anims.iter().any(|a| now(ctx) - a.since < GLOW_SECS) {
        ctx.request_repaint();
    } else {
        app.chat.anims.clear();
    }
}

/// Escape closes; so does a click outside while nothing is typed, said or selected.
fn close_keys(app: &mut WordApp, ctx: &egui::Context, input_id: egui::Id) {
    let focused = ctx.memory(|m| m.has_focus(input_id));
    if (focused || app.escape_free) && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        app.chat.open = false;
        return;
    }
    let clicked_outside =
        ctx.input(|i| i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| app.chat.last_rect.is_none_or(|r| !r.contains(p))));
    if clicked_outside && app.chat.input.trim().is_empty() && app.chat.messages.is_empty() && app.session.sel.is_collapsed() {
        app.chat.open = false;
    }
}

/// The closed state: a round button with the spark.
fn closed_button(app: &mut WordApp, ui: &mut egui::Ui, tok: &Tokens) {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(BUTTON), Sense::click());
    let resp = resp.on_hover_text(t("Chat with Claude (Ctrl+Alt+K)"));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, t("Open Claude chat")));
    let fill = if resp.is_pointer_button_down_on() { tok.ai_accent.gamma_multiply(0.85) } else { tok.ai_accent };
    let p = ui.painter();
    p.circle_filled(r.center() + vec2(0.0, 1.5), BUTTON / 2.0, tok.page_shadow);
    p.circle_filled(r.center(), BUTTON / 2.0, fill);
    spark(p, r.center(), 13.0, tok.ai_mark);
    if resp.has_focus() {
        p.circle_stroke(r.center(), BUTTON / 2.0 + 3.0, Stroke::new(2.0, tok.ai_accent));
    }
    if resp.clicked() {
        app.chat.toggle();
    }
}

/// An original mark drawn in code: a burst of sixteen rounded rays around an open centre.
fn spark(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let w = (r * 0.16).max(1.5);
    let reach = r - w / 2.0;
    for k in 0..16 {
        let a = k as f32 * std::f32::consts::TAU / 16.0;
        let d = vec2(a.cos(), a.sin());
        let (a0, a1) = (c + d * (reach * 0.22), c + d * reach);
        p.line_segment([a0, a1], Stroke::new(w, color));
        p.circle_filled(a0, w / 2.0, color);
        p.circle_filled(a1, w / 2.0, color);
    }
}

/// The rounded square with the conversation, its newest message at the bottom.
fn conversation(app: &mut WordApp, ui: &mut egui::Ui, tok: &Tokens, width: f32, area: Rect) {
    let side = width.min(area.height() - BUTTON * 3.0 - MARGIN * 2.0 - STATUS_BAR).max(160.0);
    egui::Frame::new()
        .fill(tok.panel)
        .stroke(Stroke::new(1.0, tok.border))
        .corner_radius(CornerRadius::same(16))
        .shadow(egui::epaint::Shadow { offset: [0, 4], blur: 16, spread: 0, color: tok.page_shadow })
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(width - 24.0);
            ui.set_height(side - 24.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Claude").strong().color(tok.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let b = ui.add_sized([96.0, 32.0], egui::Button::new(t("New chat")));
                    if b.clicked() {
                        app.chat.new_chat();
                        let _ = app.run("ai.discard", json!({}));
                    }
                });
            });
            ui.separator();
            let avail = ui.available_height();
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).max_height(avail).show(ui, |ui| {
                // Bottom-aligned: a short conversation sits on the square's floor.
                ui.add_space((avail - app.chat.content_h - 8.0).max(0.0));
                let top = ui.cursor().min.y;
                let n = app.chat.messages.len();
                for i in 0..n {
                    let Some(m) = app.chat.messages.get(i).cloned() else { continue };
                    message(ui, tok, &m, width - 40.0);
                    ui.add_space(6.0);
                }
                if let Some(s) = app.chat.status.clone() {
                    ui.label(egui::RichText::new(s).italics().color(tok.text_dim));
                }
                if !app.chat.busy() {
                    proposal(app, ui, tok);
                }
                app.chat.content_h = ui.cursor().min.y - top;
            });
        });
}

fn message(ui: &mut egui::Ui, tok: &Tokens, m: &Message, max: f32) {
    let layout = if m.from_user { Layout::top_down(Align::Max) } else { Layout::top_down(Align::Min) };
    ui.with_layout(layout, |ui| {
        let fill = if m.from_user { tok.ai_accent.gamma_multiply(0.14) } else { Color32::TRANSPARENT };
        egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(10)).inner_margin(if m.from_user { 8 } else { 0 }).show(ui, |ui| {
            ui.set_max_width(max);
            ui.add(egui::Label::new(egui::RichText::new(&m.text).color(tok.text)).wrap());
        });
    });
}

/// The pending proposal: each edit (old struck through, new text) with Apply and Discard.
fn proposal(app: &mut WordApp, ui: &mut egui::Ui, tok: &Tokens) {
    let Some(p) = app.session.doc_ai.proposal.clone() else { return };
    egui::Frame::new().stroke(Stroke::new(1.0, tok.ai_accent)).corner_radius(CornerRadius::same(10)).inner_margin(8).show(ui, |ui| {
        ui.label(egui::RichText::new(t("Suggested changes")).strong().color(tok.text));
        if !p.summary.is_empty() {
            ui.label(egui::RichText::new(&p.summary).color(tok.text_dim));
        }
        for e in p.edits.iter().take(20) {
            if !e.old.is_empty() {
                ui.label(egui::RichText::new(short(&e.old, 160)).strikethrough().color(tok.text_dim));
            }
            if !e.text.is_empty() {
                ui.label(egui::RichText::new(short(&e.text, 400)).color(tok.ai_accent_text));
            }
            ui.add_space(4.0);
        }
        if p.edits.len() > 20 {
            ui.label(format!("… +{}", p.edits.len() - 20));
        }
        ui.horizontal(|ui| {
            let apply = ui.add_sized([120.0, 36.0], egui::Button::new(egui::RichText::new(t("Apply")).color(tok.on_ai_accent)).fill(tok.ai_accent));
            let discard = ui.add_sized([120.0, 36.0], egui::Button::new(t("Discard")));
            if apply.clicked() {
                run_ai(app, "ai.apply");
            }
            if discard.clicked() {
                run_ai(app, "ai.discard");
            }
        });
    });
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// The input pill: the reference chip, the text box (grows upwards to eight lines, then
/// scrolls) and the send button.
fn pill(app: &mut WordApp, ui: &mut egui::Ui, ctx: &egui::Context, tok: &Tokens, width: f32, input_id: egui::Id) {
    let row = ui.text_style_height(&egui::TextStyle::Body);
    let lines = app.chat.input.lines().count().max(1) as f32 + if app.chat.input.ends_with('\n') { 1.0 } else { 0.0 };
    let multi = lines > 1.0 || app.chat.reference.is_some();
    egui::Frame::new()
        .fill(tok.input)
        .stroke(Stroke::new(1.0, tok.input_border))
        .corner_radius(CornerRadius::same(if multi { 18 } else { 24 }))
        .shadow(egui::epaint::Shadow { offset: [0, 2], blur: 10, spread: 0, color: tok.page_shadow })
        .inner_margin(egui::Margin::symmetric(14, 7))
        .show(ui, |ui| {
            ui.set_width(width - 28.0);
            if let Some(r) = app.chat.reference.clone() {
                ui.horizontal(|ui| {
                    let label = format!("{} „{}“", t("About:"), short(r.text.trim(), CHIP_CHARS));
                    ui.label(egui::RichText::new(label).small().color(tok.ai_accent_text));
                    // Drawn, not a glyph: the interface fonts may not have one for ✕.
                    let (xr, x) = ui.allocate_exact_size(vec2(32.0, 28.0), Sense::click());
                    let c = xr.center();
                    let col = if x.hovered() || x.has_focus() { tok.text } else { tok.text_dim };
                    ui.painter().line_segment([c + vec2(-4.0, -4.0), c + vec2(4.0, 4.0)], Stroke::new(1.5, col));
                    ui.painter().line_segment([c + vec2(4.0, -4.0), c + vec2(-4.0, 4.0)], Stroke::new(1.5, col));
                    if x.has_focus() {
                        ui.painter().rect_stroke(xr, 4.0, Stroke::new(2.0, tok.ai_accent), egui::StrokeKind::Inside);
                    }
                    let x = x.on_hover_text(t("Remove reference"));
                    x.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, t("Remove reference")));
                    if x.clicked() {
                        app.chat.dismissed = Some((r.anchor.clone(), r.focus.clone()));
                        app.chat.reference = None;
                    }
                });
            }
            ui.horizontal(|ui| {
                let send_w = 36.0;
                // Enter sends, Shift+Enter is a new line: take plain Enter before the text box.
                let focused = ctx.memory(|m| m.has_focus(input_id));
                let enter = focused && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                let h = (lines.min(MAX_LINES) * row).max(row);
                let hint = if app.chat.busy() { t("Claude is answering…") } else { t("Ask Claude or describe a change…") };
                egui::ScrollArea::vertical().id_salt("claude_chat_scroll").max_height(MAX_LINES * row).stick_to_bottom(true).show(ui, |ui| {
                    let edit = egui::TextEdit::multiline(&mut app.chat.input)
                        .id(input_id)
                        .frame(egui::Frame::NONE)
                        .desired_rows(1)
                        .min_size(vec2(0.0, h))
                        .desired_width(width - 28.0 - send_w - 12.0)
                        .hint_text(hint);
                    let resp = ui.add(edit);
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, t("Message to Claude")));
                    if app.chat.focus_input {
                        resp.request_focus();
                        app.chat.focus_input = false;
                    }
                });
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    let (r, b) = ui.allocate_exact_size(Vec2::splat(send_w), Sense::click());
                    let enabled = !app.chat.input.trim().is_empty() && !app.chat.busy();
                    b.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, t("Send")));
                    let b = b.on_hover_text(t("Send (Enter)"));
                    let fill = if enabled { tok.ai_accent } else { tok.text_disabled };
                    ui.painter().circle_filled(r.center(), send_w / 2.0, fill);
                    let c = r.center();
                    ui.painter().line_segment([c + vec2(0.0, 7.0), c + vec2(0.0, -7.0)], Stroke::new(2.0, tok.on_ai_accent));
                    ui.painter().line(vec![c + vec2(-5.0, -2.0), c + vec2(0.0, -7.0), c + vec2(5.0, -2.0)], Stroke::new(2.0, tok.on_ai_accent));
                    if b.has_focus() {
                        ui.painter().circle_stroke(c, send_w / 2.0 + 3.0, Stroke::new(2.0, tok.ai_accent));
                    }
                    if (b.clicked() || enter) && enabled {
                        send(app, ctx);
                        app.chat.focus_input = true;
                    }
                });
            });
        });
}

/// Paint the chat's marks on the pages: the reference (while open), a proposal's ranges, and
/// applied edits revealing and glowing. `rects` are the pages on screen, `scale` points to pixels.
pub fn paint_doc(app: &WordApp, painter: &egui::Painter, layout: &wordcraft_engine::layout::DocLayout, rects: &[Rect], scale: f32, tok: &Tokens) {
    let doc = &app.session.doc;
    let hint = app.session.page_hint;
    let spans = |a: &Pos, b: &Pos| -> Vec<Rect> {
        let (a, b) = if a <= b { (doc.clamp(a), doc.clamp(b)) } else { (doc.clamp(b), doc.clamp(a)) };
        layout
            .selection_rects(doc, &a, &b, hint)
            .into_iter()
            .filter_map(|(pi, r)| {
                rects.get(pi).map(|pr| Rect::from_min_size(pos2(pr.min.x + r.x * scale, pr.min.y + r.y * scale), vec2(r.w * scale, r.h * scale)))
            })
            .collect()
    };
    if app.chat.open
        && let Some(r) = &app.chat.reference
    {
        for sr in spans(&r.anchor, &r.focus) {
            painter.rect_filled(sr, 2.0, tok.ai_accent.gamma_multiply(0.12));
            painter.hline(sr.x_range(), sr.max.y - 0.5, Stroke::new(1.5, tok.ai_accent.gamma_multiply(0.7)));
        }
    }
    if let Some(p) = &app.session.doc_ai.proposal {
        for e in &p.edits {
            for sr in spans(&e.anchor, &e.focus) {
                painter.rect_filled(sr, 2.0, tok.ai_accent.gamma_multiply(0.10));
                painter.hline(sr.x_range(), sr.center().y, Stroke::new(1.0, tok.text_dim));
            }
        }
    }
    let motion = app.animations_on();
    let now = painter.ctx().input(|i| i.time);
    // The page colour the canvas paints, so the reveal's cover matches it.
    let view = &app.session.view;
    let paper = if view.read_mode {
        match view.read_page_color {
            wordcraft_engine::cmd::view::ReadPageColor::Inverse => Color32::from_gray(wordcraft_engine::render::DARK_PAPER),
            wordcraft_engine::cmd::view::ReadPageColor::Sepia => {
                let s = wordcraft_engine::render::SEPIA_PAPER;
                Color32::from_rgb(s.0, s.1, s.2)
            }
            _ => Color32::WHITE,
        }
    } else if view.dark_mode {
        Color32::from_gray(wordcraft_engine::render::DARK_PAPER)
    } else {
        Color32::WHITE
    };
    for a in &app.chat.anims {
        let age = now - a.since;
        let shown = spans(&a.start, &a.end);
        // The glow: strongest at once, gone after GLOW_SECS.
        let glow = (1.0 - (age / GLOW_SECS)).clamp(0.0, 1.0) as f32;
        for sr in &shown {
            painter.rect_filled(sr.expand(1.5), 3.0, tok.ai_accent.gamma_multiply(0.28 * glow));
        }
        if !motion {
            continue;
        }
        // The reveal: new text is uncovered left to right, line after line.
        let total: f32 = shown.iter().map(|r| r.width()).sum();
        let mut left = total * ((age / REVEAL_SECS).clamp(0.0, 1.0) as f32);
        for sr in &shown {
            let seen = left.min(sr.width());
            left -= seen;
            if seen < sr.width() {
                let cover = Rect::from_min_max(pos2(sr.min.x + seen, sr.min.y), sr.max);
                painter.rect_filled(cover, 0.0, paper.gamma_multiply(0.92));
            }
        }
    }
}
