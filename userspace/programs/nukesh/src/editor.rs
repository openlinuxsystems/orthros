// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The line editor.
//!
//! At a terminal, reedline reads each command: Emacs or vi keys (`set -o
//! emacs`, `set -o vi`), the history (Up, Down, Ctrl-R), programmable
//! completion on Tab, and a command that goes on past its line (an open
//! quote, `if` with no `fi`, a here-document) read as one, with `PS2`
//! before each line after the first. A dumb terminal (`TERM=dumb`, or no
//! `TERM`) and input that is not a terminal get a plain reader: the
//! prompt, then a line, read a byte at a time so that nothing after it is
//! taken from the input.
//!
//! With colours (colors.toml(5), ADR 0258), the line is coloured as it is
//! typed (`highlight`), a command from the history that starts with what
//! was typed shows faded after the cursor (ADR 0259), and while `PS1` is
//! left at its default the prompt is `name@host folder $`, with a badge
//! for a failed command's status and one while the shell is root's.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use reedline::{
    Color, ColumnarMenu, Completer, CompletionResult, DescriptionPosition, EditCommand, Emacs,
    Highlighter, HistoryItem, KeyCode, KeyModifiers, Keybindings, ListMenu, MenuBuilder, Prompt,
    PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Span, StyledText, Suggestion, ValidationResult, Validator, Vi,
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    default_vi_visual_keybindings,
};

use crate::colors::Colors;
use crate::highlight;
use crate::parse;
use crate::shell::Shell;
use crate::{prompt, sys, text, trap};

/// What the editor read.
pub enum Input {
    /// A command's text: one line, or several when the first did not
    /// finish it.
    Command(String),
    /// Ctrl-C: the line was dropped.
    Interrupted,
    /// The end of the input: Ctrl-D on an empty line, or the end of a file
    /// or pipe.
    Eof,
    /// `TMOUT` seconds went by with nothing typed.
    TimedOut,
    /// A signal with a trap came while the prompt waited; the line typed so
    /// far is kept for the next prompt.
    Signal,
}

/// The shell, as reedline's completer and validator hold it. reedline
/// asks for parts that are Send, so that it could move them to another
/// thread; it calls them only from `read_line`, on the thread that runs
/// the shell, and never moves them.
use crate::assist::Handle;

/// The line editor for an interactive shell.
pub struct Editor {
    shell: Rc<RefCell<Shell>>,
    /// reedline, at a terminal that can take it.
    line: Option<Reedline>,
    /// Whether `line` has vi keys, and Space for abbreviations.
    vi: bool,
    abbrs: bool,
    /// Whether `line` wakes each second to look at the clock and at
    /// signals: only for `TMOUT` and traps, so that an idle prompt costs
    /// nothing otherwise.
    wakes: bool,
    /// Set to make `read_line` return, and the time (in milliseconds since
    /// 1970) at which `TMOUT` runs out, or 0.
    wake: Arc<AtomicBool>,
    deadline: Arc<AtomicU64>,
    /// The shell's history as reedline has it: the history's generation
    /// and count when it was last given to reedline.
    synced: Option<(u64, usize)>,
    /// The line reedline read last, which it added to its own history.
    submitted: Option<String>,
}

impl Editor {
    pub fn new(shell: Rc<RefCell<Shell>>) -> Editor {
        let fancy = can_edit(&shell.borrow());
        let mut editor = Editor {
            shell,
            line: None,
            vi: false,
            abbrs: false,
            wakes: false,
            wake: Arc::new(AtomicBool::new(false)),
            deadline: Arc::new(AtomicU64::new(0)),
            synced: None,
            submitted: None,
        };
        if fancy {
            editor.build();
        }
        editor
    }

    /// Makes reedline for the shell's settings now.
    fn build(&mut self) {
        let (vi, abbrs, colors, wakes, palette) = {
            let shell = self.shell.borrow();
            let abbrs = !shell.abbrs.is_empty();
            (shell.opts.vi, abbrs, colors(&shell), needs_wakes(&shell), shell.colors.clone())
        };
        let handle = Handle(self.shell.clone());
        let (editor, file) = buffer_editor(&self.shell.borrow());
        let mut line = new_reedline(vi, abbrs, colors).with_buffer_editor(editor, file);
        // OSC 133 marks around the prompt and the line typed, so that
        // terminals can jump between commands (ADR 0272); the marks of a
        // command's output are written around it (interactive.rs).
        if self.shell.borrow().settings.marks {
            line = line.with_semantic_markers(Some(reedline::Osc133Markers::boxed()));
        }
        if colors && palette.on("line") {
            line = line.with_highlighter(Box::new(ShellHighlighter {
                shell: handle.clone(),
                colors: (*palette).clone(),
                known: RefCell::new((String::new(), HashMap::new())),
            }));
        }
        // A hint shows only in its colour: without colours it would look
        // typed.
        if colors && (palette.history_hints || palette.option_help) {
            let style = palette.style("line.hint").to_reedline();
            line = line.with_hinter(Box::new(crate::assist::ShellHinter::new(
                handle.clone(),
                style,
                palette.history_hints,
                palette.option_help,
            )));
        }
        let mut line = line
            .with_completer(Box::new(ShellCompleter(handle.clone())))
            .with_validator(Box::new(ShellValidator(handle.clone())))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                ColumnarMenu::default().with_name(MENU),
            )))
            .with_menu(ReedlineMenu::WithCompleter {
                menu: Box::new(
                    ListMenu::default()
                        .with_name(PICKER)
                        .with_marker("search: ")
                        .with_only_buffer_difference(false)
                        .with_page_size(8)
                        .with_description_position(DescriptionPosition::After),
                ),
                completer: Box::new(crate::assist::Picker(handle.clone())),
            })
            .with_quick_completions(true)
            .with_partial_completions(true);
        if wakes {
            let (wake, deadline) = (self.wake.clone(), self.deadline.clone());
            line = line
                .with_break_signal(self.wake.clone())
                .with_poll_interval(Duration::from_secs(1))
                .with_idle_callback(Box::new(move || {
                    let due = deadline.load(Ordering::Relaxed);
                    if trap::pending_other_than(libc::SIGWINCH) || (due != 0 && now_millis() >= due)
                    {
                        wake.store(true, Ordering::Relaxed);
                    }
                }));
        }
        self.line = Some(line);
        self.vi = vi;
        self.abbrs = abbrs;
        self.wakes = wakes;
        // The new editor's history is empty.
        self.synced = None;
    }

    /// Shows the prompt (`PS1` expanded) and reads the next command.
    pub fn read_command(&mut self) -> Input {
        loop {
            match self.read_once() {
                Ok(input) => return input,
                Err(key) => self.host_key(&key),
            }
        }
    }

    /// One of the keys the shell handles itself (ADR 0262): Space after
    /// an abbreviation, and Alt+Left and Alt+Right on an empty line.
    fn host_key(&mut self, key: &str) {
        let Some(line) = self.line.as_mut() else { return };
        let buffer = line.current_buffer_contents().to_string();
        let cursor = line.current_insertion_point();
        match key {
            KEY_SPACE => {
                let grown = crate::abbr::grow(&self.shell.borrow(), &buffer, cursor);
                if let Some((text, at)) = grown {
                    line.run_edit_commands(&[
                        EditCommand::Clear,
                        EditCommand::InsertString(text),
                        EditCommand::MoveToPosition { position: at, select: false },
                    ]);
                }
                line.run_edit_commands(&[EditCommand::InsertChar(' ')]);
            }
            KEY_ELEVATE => {
                // Alt+S: elevate(1) before the line, or off it again, as
                // fish's Alt+S does with sudo; the cursor stays on its text.
                let (text, at) = match buffer.strip_prefix("elevate ") {
                    Some(rest) => (rest.to_string(), cursor.saturating_sub(8)),
                    None => (format!("elevate {buffer}"), cursor + 8),
                };
                line.run_edit_commands(&[
                    EditCommand::Clear,
                    EditCommand::InsertString(text),
                    EditCommand::MoveToPosition { position: at, select: false },
                ]);
            }
            KEY_EXPLAIN => {
                // Alt+E: what the line does, under it; with an empty
                // line, why the last command failed (ADR 0268). The line
                // is kept, and the prompt is drawn again below.
                let mut shell = self.shell.borrow_mut();
                let paint = colors(&shell);
                let text = crate::explain::for_prompt(&mut shell, &buffer, paint);
                let text = if text.is_empty() {
                    "Type a command, then Alt+E says what it does.\n".to_string()
                } else {
                    text
                };
                let shown = format!("\n{}", text).replace('\n', "\r\n");
                let _ = sys::write_all(1, shown.as_bytes());
            }
            KEY_BACK | KEY_FORWARD if !buffer.is_empty() => {
                let command = if key == KEY_BACK {
                    EditCommand::MoveWordLeft { select: false }
                } else {
                    EditCommand::MoveWordRight { select: false }
                };
                line.run_edit_commands(&[command]);
            }
            KEY_BACK | KEY_FORWARD => {
                let mut shell = self.shell.borrow_mut();
                if crate::folders::step(&mut shell, key == KEY_BACK) {
                    shell.status = 0;
                }
            }
            _ => {}
        }
    }

    /// Reads once: a command, or a key the shell handles, with the line
    /// kept for the next read.
    fn read_once(&mut self) -> Result<Input, String> {
        let (ps1, ps2, vi, wakes, timeout) = {
            let mut shell = self.shell.borrow_mut();
            let ps1 = shell.get("PS1").unwrap_or_default();
            let ps1 = prompt::expand(&mut shell, &ps1);
            let ps2 = shell.get("PS2").unwrap_or_default();
            let ps2 = prompt::expand(&mut shell, &ps2);
            (ps1, ps2, shell.opts.vi, needs_wakes(&shell), timeout(&shell))
        };
        if self.line.is_none() {
            return Ok(self.read_plain(&ps1, timeout));
        }
        let (ps1, right, abbrs) = {
            let mut shell = self.shell.borrow_mut();
            if let Some(records) = shell.records.as_mut() {
                records.refresh();
            }
            let rich = rich_prompt(&mut shell);
            let right = if rich.is_some() && !shell.settings.screen_reader {
                crate::assist::right_prompt(&shell, colors(&shell))
            } else {
                String::new()
            };
            (rich.unwrap_or(ps1), right, !shell.abbrs.is_empty())
        };
        if wakes != self.wakes || abbrs != self.abbrs {
            self.build();
        } else if vi != self.vi {
            self.vi = vi;
            let line = self.line.take().map(|line| line.with_edit_mode(edit_mode(vi, abbrs)));
            self.line = line;
        }
        self.sync_history();
        let due = timeout.map_or(0, |t| now_millis() + t.as_millis() as u64);
        self.deadline.store(due, Ordering::Relaxed);
        let prompt = ShellPrompt { left: ps1, right, more: ps2 };
        let Some(line) = self.line.as_mut() else { return Ok(Input::Eof) };
        crate::hangup::enter_prompt(&self.shell.borrow());
        let read = line.read_line(&prompt);
        crate::hangup::leave_prompt();
        Ok(match read {
            Ok(Signal::HostCommand(key)) => return Err(key),
            Ok(Signal::Success(text)) => {
                self.submitted = Some(text.clone());
                Input::Command(text)
            }
            Ok(Signal::CtrlC) => {
                // As if SIGINT had come at the prompt, so that its trap runs.
                trap::raise_later(libc::SIGINT);
                Input::Interrupted
            }
            Ok(Signal::ExternalBreak(_)) => {
                if trap::pending_other_than(libc::SIGWINCH) {
                    Input::Signal
                } else {
                    Input::TimedOut
                }
            }
            Ok(_) => Input::Eof,
            Err(error) => {
                // A terminal that went away: the end of the input.
                let _ = sys::write_all(
                    2,
                    text::encode(&format!("nukesh: line editor: {error}\n")).as_slice(),
                );
                Input::Eof
            }
        })
    }

    /// Reads a command without reedline: a line, and more while the
    /// command is not finished, each after `PS2`.
    fn read_plain(&mut self, ps1: &str, timeout: Option<Duration>) -> Input {
        let _ = sys::write_all(2, &text::encode(ps1));
        if let Some(timeout) = timeout {
            match sys::poll_in(0, timeout.as_millis().min(i32::MAX as u128) as i32) {
                Ok(true) => {}
                Ok(false) => return Input::TimedOut,
                Err(_) if trap::is_pending(libc::SIGINT) => {
                    let _ = sys::write_all(2, b"\n");
                    return Input::Interrupted;
                }
                Err(_) => return Input::Signal,
            }
        }
        let mut command = match read_plain_line(0) {
            Got::Line(line) => line,
            Got::Eof => return Input::Eof,
            Got::Interrupted => {
                let _ = sys::write_all(2, b"\n");
                return Input::Interrupted;
            }
        };
        loop {
            let options = self.shell.borrow().parse_options();
            if !needs_more(&command, options) {
                return Input::Command(command);
            }
            let ps2 = {
                let mut shell = self.shell.borrow_mut();
                let ps2 = shell.get("PS2").unwrap_or_default();
                prompt::expand(&mut shell, &ps2)
            };
            let _ = sys::write_all(2, &text::encode(&ps2));
            match read_plain_line(0) {
                Got::Line(line) => {
                    command.push('\n');
                    command.push_str(&line);
                }
                // The parser reports the unfinished command.
                Got::Eof => return Input::Command(command),
                Got::Interrupted => {
                    let _ = sys::write_all(2, b"\n");
                    return Input::Interrupted;
                }
            }
        }
    }

    /// Gives reedline the shell's history again when it changed other
    /// than by the line reedline read last, which it keeps on its own.
    fn sync_history(&mut self) {
        let Some(line) = self.line.as_mut() else { return };
        let shell = self.shell.borrow();
        let history = &shell.history;
        let now = (history.generation, history.count());
        let submitted = self.submitted.take();
        let in_step = match (self.synced, submitted) {
            (Some(then), None) => then == now,
            // An empty line is not added to either.
            (Some(then), Some(text)) if text.trim().is_empty() => then == now,
            (Some((generation, count)), Some(text)) => {
                generation == now.0
                    && count + 1 == now.1
                    && history.entries.last().is_some_and(|entry| entry.line == text)
            }
            (None, _) => false,
        };
        if !in_step {
            let editor_history = line.history_mut();
            let _ = editor_history.clear();
            for entry in &history.entries {
                let _ = editor_history.save(HistoryItem::from_command_line(entry.line.clone()));
            }
        }
        self.synced = Some(now);
    }
}

/// The completion menu's name, which Tab opens.
const MENU: &str = "completion_menu";

/// The picker's, which Ctrl+R opens.
const PICKER: &str = "command_picker";

/// The keys the shell handles itself, as reedline hands them back.
const KEY_SPACE: &str = "nukesh:space";
const KEY_BACK: &str = "nukesh:back";
const KEY_FORWARD: &str = "nukesh:forward";
const KEY_ELEVATE: &str = "nukesh:elevate";
const KEY_EXPLAIN: &str = "nukesh:explain";

/// Whether reedline can run: input and output at a terminal that is not
/// dumb.
fn can_edit(shell: &Shell) -> bool {
    let term = shell.get("TERM").unwrap_or_default();
    sys::isatty(0) && sys::isatty(2) && !term.is_empty() && term != "dumb"
}

/// Whether the prompt has to wake: for `TMOUT`, and for traps on signals
/// that can come while it waits.
fn needs_wakes(shell: &Shell) -> bool {
    timeout(shell).is_some()
        || trap::SIGNALS.iter().any(|(_, signal)| {
            !matches!(*signal, libc::SIGWINCH | libc::SIGCHLD)
                && shell.trap_command(*signal as usize).is_some()
        })
}

/// `TMOUT`: how long the prompt waits before the shell exits.
fn timeout(shell: &Shell) -> Option<Duration> {
    let seconds = shell.get("TMOUT")?.trim().parse::<f64>().ok()?;
    (seconds > 0.0 && seconds.is_finite()).then(|| Duration::from_secs_f64(seconds))
}

/// The time, in milliseconds since 1970.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Whether the editor may use colour: not with `NO_COLOR` set, nor with
/// colors.toml's `enabled = false`. The editor runs only at a terminal
/// that is not dumb.
fn colors(shell: &Shell) -> bool {
    shell.get("NO_COLOR").is_none_or(|v| v.is_empty()) && shell.colors.enabled && !shell.settings.screen_reader
}

/// The prompt drawn while `PS1` is its default, `$ ` or `# ` (ADR 0258):
/// `name@host folder $ `, with `✗ status` after a failed command and
/// `admin` while the shell is root's, then `#`. Colours are added when
/// they may be used.
fn rich_prompt(shell: &mut Shell) -> Option<String> {
    let ps1 = shell.get("PS1").unwrap_or_default();
    if !matches!(ps1.as_str(), "$ " | "# ") {
        return None;
    }
    if shell.settings.screen_reader {
        return Some(spoken_prompt(shell));
    }
    if !shell.colors.enabled || !shell.colors.on("prompt") {
        return None;
    }
    let palette = shell.colors.clone();
    let paint = colors(shell);
    let style = |key: &str, text: &str| {
        if paint { palette.style(&format!("prompt.{key}")).paint(text) } else { text.to_string() }
    };
    // The name, looked up once: it is the same at every prompt.
    thread_local!(static USER: std::cell::OnceCell<String> = const { std::cell::OnceCell::new() });
    let user = USER.with(|cell| cell.get_or_init(|| prompt::expand(shell, "\\u")).clone());
    let host = prompt::expand(shell, "\\h");
    let path = prompt::expand(shell, "\\w");
    let mut out = format!(
        "{}{}{} {}",
        style("user", &user),
        style("at", "@"),
        style("host", &host),
        style("path", &path)
    );
    if shell.status != 0 {
        out.push(' ');
        out.push_str(&style("error", &format!(" ✗ {} ", shell.status)));
    }
    let root = sys::geteuid() == 0;
    if root {
        out.push(' ');
        out.push_str(&style("admin", " admin "));
    }
    if shell.private {
        out.push(' ');
        out.push_str(&style("private", " private "));
    }
    out.push(' ');
    out.push_str(&style("symbol", if root { "#" } else { "$" }));
    out.push(' ');
    Some(out)
}

/// The prompt in screen-reader mode (ADR 0269): words a reader says
/// well, and nothing drawn: `~/src, failed 1, admin # `.
fn spoken_prompt(shell: &mut Shell) -> String {
    let mut out = prompt::expand(shell, "\\w");
    if shell.status != 0 {
        out.push_str(&format!(", failed {}", shell.status));
    }
    let root = sys::geteuid() == 0;
    if root {
        out.push_str(", admin");
    }
    if shell.private {
        out.push_str(", private");
    }
    out.push_str(if root { " # " } else { " $ " });
    out
}

/// reedline with the keys for `vi` or Emacs, and no colouring of its own
/// for the line typed.
fn new_reedline(vi: bool, abbrs: bool, colors: bool) -> Reedline {
    Reedline::create()
        .with_edit_mode(edit_mode(vi, abbrs))
        .with_highlighter(Box::new(Plain))
        .with_ansi_colors(colors)
        .use_bracketed_paste(true)
}

/// The keys: Emacs or vi, with Tab for completion, Ctrl+R for the
/// picker, Alt+Left and Alt+Right for the folders, and Space for
/// abbreviations when there are any.
fn edit_mode(vi: bool, abbrs: bool) -> Box<dyn reedline::EditMode> {
    let complete = |keys: &mut Keybindings| {
        keys.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('r'),
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(PICKER.to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
        let host = |key: &str| ReedlineEvent::ExecuteHostCommand(key.to_string());
        keys.add_binding(KeyModifiers::ALT, KeyCode::Left, host(KEY_BACK));
        keys.add_binding(KeyModifiers::ALT, KeyCode::Right, host(KEY_FORWARD));
        keys.add_binding(KeyModifiers::ALT, KeyCode::Char('s'), host(KEY_ELEVATE));
        keys.add_binding(KeyModifiers::ALT, KeyCode::Char('e'), host(KEY_EXPLAIN));
        if abbrs {
            keys.add_binding(KeyModifiers::NONE, KeyCode::Char(' '), host(KEY_SPACE));
        }
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(MENU.to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keys.add_binding(KeyModifiers::SHIFT, KeyCode::BackTab, ReedlineEvent::MenuPrevious);
    };
    if vi {
        let mut insert = default_vi_insert_keybindings();
        complete(&mut insert);
        Box::new(Vi::new(insert, default_vi_normal_keybindings(), default_vi_visual_keybindings()))
    } else {
        let mut keys = default_emacs_keybindings();
        complete(&mut keys);
        // Ctrl-X Ctrl-E with Emacs keys, as bash has it; vi keys switch
        // modes inside reedline, so they are left as they are.
        Box::new(Chords { inner: Box::new(Emacs::new(keys)), after_ctrl_x: false })
    }
}

/// The Emacs keys, with bash's Ctrl-X Ctrl-E, which opens the line in an
/// editor: reedline binds single keys only.
struct Chords {
    inner: Box<Emacs>,
    after_ctrl_x: bool,
}

impl reedline::EditMode for Chords {
    fn parse_event(&mut self, event: reedline::ReedlineRawEvent) -> ReedlineEvent {
        use crossterm::event::{Event, KeyCode as Code, KeyEvent, KeyModifiers as Mods};
        let raw: Event = event.into();
        if let Event::Key(KeyEvent { code: Code::Char(c), modifiers, .. }) = &raw
            && modifiers.contains(Mods::CONTROL)
        {
            let after = std::mem::take(&mut self.after_ctrl_x);
            match c.to_ascii_lowercase() {
                'e' if after => return ReedlineEvent::OpenEditor,
                'x' if !after => {
                    self.after_ctrl_x = true;
                    return ReedlineEvent::None;
                }
                _ => {}
            }
        } else {
            self.after_ctrl_x = false;
        }
        match reedline::ReedlineRawEvent::try_from(raw) {
            Ok(event) => self.inner.parse_event(event),
            Err(()) => ReedlineEvent::None,
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

/// The editor Ctrl-X Ctrl-E and Ctrl-O open the line in: `VISUAL`, else
/// `EDITOR`, else nano; its words split as the shell would not need to.
fn buffer_editor(shell: &Shell) -> (std::process::Command, std::path::PathBuf) {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| shell.get(name).filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "nano".into());
    let mut words = editor.split_whitespace();
    let mut command = std::process::Command::new(words.next().unwrap_or("nano"));
    command.args(words);
    let dir = shell.get("TMPDIR").filter(|d| d.starts_with('/')).unwrap_or_else(|| "/tmp".into());
    let file = std::path::PathBuf::from(format!("{dir}/nukesh-line-{}.sh", std::process::id()));
    (command, file)
}

/// The line as typed, without colours.
struct Plain;

impl Highlighter for Plain {
    // The style's type is nu-ansi-term's, which reedline does not export.
    #[allow(clippy::default_trait_access)]
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::default();
        styled.buffer.push((Default::default(), line.to_string()));
        styled
    }
}

/// The line as typed, coloured by what each piece is (`highlight`).
struct ShellHighlighter {
    shell: Handle,
    colors: Colors,
    /// Whether each command name runs something, for the `PATH` and the
    /// history's count they were looked up with: a command run since, or
    /// a new `PATH`, may change the answers.
    known: RefCell<(String, HashMap<String, bool>)>,
}

impl ShellHighlighter {
    /// Whether `name`, as typed where a command starts, runs something:
    /// an alias, function, builtin or reserved word, a file named by a
    /// path, or a program on `PATH`.
    fn known(&self, name: &str) -> bool {
        // While the shell is busy (it never is while a line is read), the
        // benefit of the doubt.
        let Ok(shell) = self.shell.0.try_borrow() else { return true };
        let stamp = format!(
            "{}\0{}\0{}",
            shell.get("PATH").unwrap_or_default(),
            shell.history.generation,
            shell.history.count()
        );
        let mut known = self.known.borrow_mut();
        if known.0 != stamp {
            *known = (stamp, HashMap::new());
        }
        if let Some(answer) = known.1.get(name) {
            return *answer;
        }
        let answer = shell.aliases.contains_key(name)
            || shell.functions.contains_key(name)
            || crate::builtins::lookup(name)
                .is_some_and(|_| crate::builtins::enabled(&shell, name))
            || parse::is_reserved(name)
            || shell.hash.contains_key(name)
            || if name.contains('/') {
                let path = match name.strip_prefix("~/") {
                    Some(rest) => format!("{}/{rest}", shell.get("HOME").unwrap_or_default()),
                    None => name.to_string(),
                };
                std::fs::metadata(text::to_os(&path)).is_ok()
            } else {
                shell.search_path(name, false).is_some()
            };
        known.1.insert(name.to_string(), answer);
        answer
    }
}

// SAFETY: as for `Handle`: reedline calls the highlighter only from
// `read_line`, on the shell's thread.
unsafe impl Send for ShellHighlighter {}

impl Highlighter for ShellHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::default();
        // A mistake the parser finds is shown before Enter (ADR 0274): the
        // token it is near, as an unknown command is.
        let options = self.shell.0.try_borrow().map_or_default(|shell| shell.parse_options());
        let wrong = mistake(line, options.clone()).and_then(|(token, _)| token);
        let spans = highlight::spans(line, &mut |name| self.known(name));
        // The piece to mark: the first of the token's where the line up to
        // it is already wrong (`ls | | wc`: the second `|`).
        let mut marked = None;
        if let Some(token) = &wrong {
            let mut end = 0;
            for (index, (_, piece)) in spans.iter().enumerate() {
                end += piece.len();
                if piece == token && mistake(&line[..end], options.clone()).is_some() {
                    marked = Some(index);
                    break;
                }
            }
        }
        for (index, (role, piece)) in spans.into_iter().enumerate() {
            let role = if marked == Some(index) { highlight::Role::Unknown } else { role };
            let style = role.key().map_or_else(Default::default, |key| self.colors.style(key));
            styled.buffer.push((style.to_reedline(), piece));
        }
        styled
    }
}

/// The syntax mistake in `line`, if it is finished and wrong: the token
/// it is near, when the message names one, and the message.
pub fn mistake(line: &str, options: parse::Options) -> Option<(Option<String>, String)> {
    if line.trim().is_empty() {
        return None;
    }
    let mut whole = line.to_string();
    whole.push('\n');
    let mut parser = parse::Parser::new("", Rc::from("stdin"), 1, options);
    parser.push_text(&whole);
    let Err(parse::Error::Syntax { messages, .. }) = parser.parse() else { return None };
    let message = messages.into_iter().next()?.1;
    let token = message
        .split_once("unexpected token `")
        .and_then(|(_, rest)| rest.split_once('\'').map(|(t, _)| t.to_string()))
        .filter(|t| t != "newline");
    Some((token, message))
}

/// `PS1`, and `PS2` before each line after the first.
struct ShellPrompt {
    left: String,
    right: String,
    more: String,
}

impl Prompt for ShellPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.left)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.right)
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.more)
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        let failed = match search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failed ",
        };
        Cow::Owned(format!("({failed}reverse-i-search)`{}': ", search.term))
    }

    // The prompt's colours are those its escapes give.
    fn get_prompt_color(&self) -> Color {
        Color::Default
    }

    fn get_prompt_multiline_color(&self) -> Color {
        Color::Default
    }

    fn get_indicator_color(&self) -> Color {
        Color::Default
    }

    fn get_prompt_right_color(&self) -> Color {
        Color::Default
    }
}

/// Tab: the shell's programmable completion.
struct ShellCompleter(Handle);

impl Completer for ShellCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let Ok(mut shell) = self.0.0.try_borrow_mut() else {
            return CompletionResult::fresh(Vec::new());
        };
        let (start, candidates) = shell.complete_line(line, pos);
        let suggestions: Vec<Suggestion> = candidates
            .into_iter()
            .map(|candidate| Suggestion {
                display_override: (candidate.display != candidate.text)
                    .then_some(candidate.display),
                value: candidate.text,
                description: candidate.description,
                span: Span::new(start, pos),
                append_whitespace: candidate.space,
                ..Suggestion::default()
            })
            .collect();
        CompletionResult::fresh(suggestions)
    }
}

/// Enter: a command that is not finished goes on to another line.
struct ShellValidator(Handle);

impl Validator for ShellValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let options = self.0.0.try_borrow().map_or_default(|shell| shell.parse_options());
        if needs_more(line, options) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

/// Whether `text` ends inside a command, so that another line is needed:
/// an open quote or construct, a here-document without its delimiter, a
/// line that ends with `|` or `&&`, or a `\` that joins the next line.
pub fn needs_more(text: &str, options: parse::Options) -> bool {
    let last_line = text.rsplit('\n').next().unwrap_or("");
    let backslashes = last_line.chars().rev().take_while(|c| *c == '\\').count();
    if backslashes % 2 == 1 && !last_line.trim_start().starts_with('#') {
        return true;
    }
    let mut whole = text.to_string();
    whole.push('\n');
    // More text may come: a here-document without its delimiter line
    // goes on in it.
    let mut parser = parse::Parser::new("", Rc::from("stdin"), 1, options);
    parser.push_text(&whole);
    matches!(parser.parse(), Err(parse::Error::Incomplete(_)))
}

/// A line read by the plain reader.
enum Got {
    Line(String),
    Interrupted,
    Eof,
}

/// Reads a line from `fd` a byte at a time, without its newline. Ctrl-C
/// (SIGINT caught while waiting) drops it; other signals wait for the
/// line, and their traps run after it.
fn read_plain_line(fd: i32) -> Got {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: byte is valid for writes of one byte.
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        match n {
            1 if byte[0] == b'\n' => return Got::Line(text::decode(&bytes)),
            1 => bytes.push(byte[0]),
            0 => break,
            _ if sys::errno() == libc::EINTR => {
                if trap::is_pending(libc::SIGINT) {
                    return Got::Interrupted;
                }
            }
            _ => break,
        }
    }
    if bytes.is_empty() { Got::Eof } else { Got::Line(text::decode(&bytes)) }
}

/// `read -e`: one line from the editor, with `initial` already typed. The
/// end of the input is `None`.
pub fn read_line(shell: &mut Shell, prompt: &str, initial: &str) -> Option<String> {
    if !can_edit(shell) {
        let _ = sys::write_all(2, &text::encode(prompt));
        return match read_plain_line(0) {
            Got::Line(line) => Some(line),
            Got::Interrupted | Got::Eof => None,
        };
    }
    let mut line = new_reedline(shell.opts.vi, false, colors(shell));
    if !initial.is_empty() {
        line.run_edit_commands(&[EditCommand::InsertString(initial.to_string())]);
    }
    let prompt = ShellPrompt { left: prompt.to_string(), right: String::new(), more: String::new() };
    match line.read_line(&prompt) {
        Ok(Signal::Success(text)) => Some(text),
        Ok(Signal::CtrlC) => {
            trap::raise_later(libc::SIGINT);
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn more(text: &str) -> bool {
        needs_more(text, parse::Options::default())
    }

    #[test]
    fn unfinished_commands_need_more_lines() {
        assert!(more("echo 'abc"));
        assert!(more("if true; then"));
        assert!(more("echo a |"));
        assert!(more("true &&"));
        assert!(more("cat <<EOF"));
        assert!(more("cat <<EOF\nbody"));
        assert!(more("echo a \\"));
        assert!(more("f() {"));
        assert!(more("echo $(date"));
    }

    #[test]
    fn finished_commands_do_not() {
        assert!(!more("echo hi"));
        assert!(!more("cat <<EOF\nbody\nEOF"));
        assert!(!more("echo a \\\\"));
        assert!(!more("# a comment \\"));
        assert!(!more("if true; then echo; fi"));
        assert!(!more(""));
        // A syntax error is not more to read: the parser reports it.
        assert!(!more("echo )"));
    }
}
