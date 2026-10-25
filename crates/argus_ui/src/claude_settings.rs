//! The Claude section of the settings sheet (spec §10): the `claude` binary,
//! the default model and effort, whether routine permission requests are
//! allowed without asking, the review rules file and the commit-message
//! timeout.
//!
//! Text fields apply on Enter (or their Apply button). Each value is
//! validated first; an invalid one shows an inline error and is neither
//! applied nor saved. Valid changes go to the [`RunManager`]'s live settings,
//! so the next run picks them up without a restart, and are written to the
//! `settings` table on the tokio runtime. Filesystem checks (does the Claude
//! path exist and is it executable?) run there too, never on the UI thread.
//!
//! The "Claude binary" hint shows the run manager's detection of the
//! installed CLI (version, missing flags), which reruns on its own whenever
//! the path setting changes.

use std::path::PathBuf;

use argus_runs::settings::{
    self, EFFORTS, MAX_COMMIT_MESSAGE_TIMEOUT_S, MIN_COMMIT_MESSAGE_TIMEOUT_S, MODELS,
    default_review_rules_path,
};
use argus_runs::{ClaudeSettings, ClaudeStatus, RunManager, SettingError, review_rules};
use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::AppContext;
use crate::claude_status::observe_claude_status;
use crate::components::{button, radius, section_header, toggle_button};
use crate::runtime::spawn_job;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::ActiveTheme;
use crate::workspace::display_path;

/// Width of the label column of a text-field row.
const LABEL_WIDTH: f32 = 150.0;

/// The choices of the "Permissions" row: allow routine requests
/// ([`ClaudeSettings::auto_approve_safe`]), or ask for each one.
const PERMISSION_CHOICES: [&str; 2] = ["Allow routine, ask risky", "Ask every time"];

/// The editable text fields of the section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    ClaudePath,
    ReviewRules,
    CommitTimeout,
}

impl Field {
    const ALL: [Field; 3] = [Field::ClaudePath, Field::ReviewRules, Field::CommitTimeout];

    fn index(self) -> usize {
        self as usize
    }
}

/// One text field with its inline error and pending validation.
struct FieldState {
    input: Entity<TextField>,
    error: Option<SharedString>,
    /// An in-flight filesystem validation; replacing it cancels the old one.
    checking: Option<Task<()>>,
}

/// The Claude settings form, rendered inside the settings sheet.
pub struct ClaudeSettingsForm {
    settings: ClaudeSettings,
    fields: Vec<FieldState>,
    /// Follows changes of the live settings made elsewhere.
    _settings_task: Task<()>,
    /// The run manager's detection of the installed `claude`.
    claude: ClaudeStatus,
    _claude_task: Task<()>,
    /// Error of the last "Open review rules in editor" attempt.
    open_error: Option<SharedString>,
    open_task: Option<Task<()>>,
    /// The last settings write; each write waits for the previous one, so the
    /// stored record always ends up as the newest settings.
    save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ClaudeSettingsForm {
    /// Builds the form from the run manager's current settings.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = runs(cx).settings();
        let mut subscriptions = Vec::new();
        let fields = Field::ALL
            .iter()
            .map(|&field| {
                let input = cx.new(|cx| {
                    let mut input = TextField::new(placeholder(field, cx), cx);
                    input.set_text(field_text(&settings, field), cx);
                    input
                });
                subscriptions.push(cx.subscribe(
                    &input,
                    move |this, _, event: &TextFieldEvent, cx| match event {
                        TextFieldEvent::Submitted => this.submit(field, cx),
                        TextFieldEvent::Edited => this.set_error(field, None, cx),
                    },
                ));
                FieldState {
                    input,
                    error: None,
                    checking: None,
                }
            })
            .collect();
        subscriptions.push(cx.on_app_quit(|this, _| {
            let save = this.save_task.take();
            async move {
                if let Some(save) = save {
                    save.await;
                }
            }
        }));

        // Others change the live settings too (the Run menu remembers the
        // last model and effort); show their values instead of overwriting
        // them with a stale copy.
        let mut changes = runs(cx).subscribe();
        changes.borrow_and_update();
        let settings_task = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let latest = changes.borrow_and_update().clone();
                let alive = this.update(cx, |this, cx| this.adopt(latest, cx)).is_ok();
                if !alive {
                    break;
                }
            }
        });

        Self {
            settings,
            fields,
            _settings_task: settings_task,
            claude: runs(cx).claude_status(),
            _claude_task: observe_claude_status(cx, |this: &mut Self, status, _| {
                this.claude = status;
            }),
            open_error: None,
            open_task: None,
            save_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn field(&self, field: Field) -> &FieldState {
        &self.fields[field.index()]
    }

    fn field_mut(&mut self, field: Field) -> &mut FieldState {
        &mut self.fields[field.index()]
    }

    fn set_error(&mut self, field: Field, error: Option<SharedString>, cx: &mut Context<Self>) {
        let state = self.field_mut(field);
        let invalid = error.is_some();
        if state.error != error {
            state.error = error;
            cx.notify();
        }
        state
            .input
            .clone()
            .update(cx, |input, cx| input.set_invalid(invalid, cx));
    }

    /// Whether the field's text differs from the applied value.
    fn is_dirty(&self, field: Field, cx: &gpui::App) -> bool {
        self.field(field).input.read(cx).text().trim() != field_text(&self.settings, field)
    }

    // ---- applying -----------------------------------------------------------

    /// Validates the field's text and applies it, or shows the error.
    fn submit(&mut self, field: Field, cx: &mut Context<Self>) {
        let text = self.field(field).input.read(cx).text().to_owned();
        match field {
            Field::CommitTimeout => match settings::parse_commit_message_timeout(&text) {
                Ok(seconds) => self.apply(Some(field), cx, |settings| {
                    settings.commit_message_timeout_s = seconds;
                }),
                Err(error) => self.set_error(field, Some(error_text(&error)), cx),
            },
            Field::ClaudePath | Field::ReviewRules => self.submit_path(field, text, cx),
        }
    }

    /// Checks a path field on the tokio runtime, then applies it.
    fn submit_path(&mut self, field: Field, text: String, cx: &mut Context<Self>) {
        let job = spawn_job(cx, async move {
            tokio::task::spawn_blocking(move || match field {
                Field::ClaudePath => settings::validate_claude_path(&text),
                _ => settings::validate_review_rules_path(&text),
            })
            .await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.field_mut(field).checking = None;
                match result {
                    Ok(Ok(Ok(path))) => this.apply(Some(field), cx, |settings| match field {
                        Field::ClaudePath => settings.claude_path = path,
                        _ => settings.review_rules_path = path,
                    }),
                    Ok(Ok(Err(error))) => this.set_error(field, Some(error_text(&error)), cx),
                    Ok(Err(error)) => this.set_error(field, Some(error.to_string().into()), cx),
                    Err(error) => this.set_error(field, Some(error.to_string().into()), cx),
                }
                cx.notify();
            })
            .ok();
        });
        self.field_mut(field).checking = Some(task);
        cx.notify();
    }

    /// Changes the settings: publishes them to the run manager, saves them
    /// and refreshes the fields. Callers pass only validated values.
    fn apply(
        &mut self,
        submitted: Option<Field>,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut ClaudeSettings),
    ) {
        let before = self.settings.clone();
        change(&mut self.settings);
        for field in Field::ALL {
            // Other fields keep the user's in-progress text unless their own
            // value changed (a reset, or a default following its list).
            if Some(field) == submitted
                || field_text(&before, field) != field_text(&self.settings, field)
            {
                self.sync_field(field, cx);
            }
        }
        if self.settings == before {
            return;
        }
        // The run manager re-detects `claude` itself when the path changed.
        runs(cx).set_settings(self.settings.clone());
        self.save(cx);
        if self.settings.review_rules_path != before.review_rules_path {
            self.open_error = None;
        }
        cx.notify();
    }

    /// Takes over live settings changed elsewhere, refreshing only the
    /// fields whose value changed.
    fn adopt(&mut self, latest: ClaudeSettings, cx: &mut Context<Self>) {
        if latest == self.settings {
            return;
        }
        let before = std::mem::replace(&mut self.settings, latest);
        for field in Field::ALL {
            if field_text(&before, field) != field_text(&self.settings, field) {
                self.sync_field(field, cx);
            }
        }
        cx.notify();
    }

    /// Shows the applied value in the field and clears its error.
    fn sync_field(&mut self, field: Field, cx: &mut Context<Self>) {
        let text = field_text(&self.settings, field);
        let input = self.field(field).input.clone();
        input.update(cx, |input, cx| input.set_text(text, cx));
        self.field_mut(field).checking = None;
        self.set_error(field, None, cx);
    }

    /// Resets the whole section to its defaults, discarding unapplied edits.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.apply(None, cx, |settings| *settings = ClaudeSettings::default());
        for field in Field::ALL {
            self.sync_field(field, cx);
        }
    }

    /// Saves the run manager's live settings, which `apply` just updated.
    /// The manager serializes writes, so the newest settings always win,
    /// including defaults the Run menu changed.
    fn save(&mut self, cx: &mut Context<Self>) {
        let app = cx.global::<AppContext>();
        let (manager, runtime) = (app.runs.clone(), app.runtime.clone());
        let previous = self.save_task.take();
        self.save_task = Some(cx.background_executor().spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            // Not aborted when the form goes away: a started write finishes.
            let job = runtime.spawn(async move { manager.save_settings().await });
            match job.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "failed to save Claude settings"),
                Err(error) => tracing::warn!(%error, "Claude settings save task failed"),
            }
        }));
    }

    /// Opens the review rules file with the system's default application,
    /// recreating the default file first if it went missing.
    fn open_review_rules(&mut self, cx: &mut Context<Self>) {
        let path = self.settings.review_rules_path(runs(cx).argus_home());
        let is_default = self.settings.review_rules_path.is_none();
        let job = spawn_job(cx, {
            let path = path.clone();
            async move {
                if is_default {
                    review_rules::ensure_review_rules(&path)
                        .await
                        .map(drop)
                        .map_err(|error| SharedString::from(error.to_string()))
                } else {
                    match tokio::fs::metadata(&path).await {
                        Ok(metadata) if metadata.is_file() => Ok(()),
                        Ok(_) => Err(error_text(&SettingError::NotAFile(path))),
                        Err(_) => Err(error_text(&SettingError::NotFound(path))),
                    }
                }
            }
        });
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(())) => {
                        this.open_error = None;
                        // Spawns the opener on a GPUI background thread.
                        cx.open_with_system(&path);
                    }
                    Ok(Err(error)) => this.open_error = Some(error),
                    Err(error) => this.open_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- rendering ----------------------------------------------------------

    fn render_field_row(
        &self,
        field: Field,
        label: &'static str,
        trailing: Option<AnyElement>,
        hint: Option<(SharedString, bool)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let state = self.field(field);
        let dirty = self.is_dirty(field, cx);
        let checking = state.checking.is_some();
        let apply: Option<AnyElement> = if checking {
            Some(
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child("Checking…")
                    .into_any_element(),
            )
        } else if dirty {
            Some(
                button(("apply-setting", field.index()), "Apply", cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.submit(field, cx)))
                    .into_any_element(),
            )
        } else {
            None
        };
        let message = match (&state.error, hint) {
            (Some(error), _) => Some((error.clone(), true)),
            (None, hint) => hint,
        };
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .py_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .w(px(LABEL_WIDTH))
                            .text_color(colors.text)
                            .child(label),
                    )
                    .child(state.input.clone())
                    .children(apply)
                    .children(trailing),
            )
            .children(message.map(|(text, is_error)| {
                div()
                    .pl(px(LABEL_WIDTH + 8.0))
                    .text_xs()
                    .text_color(if is_error {
                        colors.error
                    } else {
                        colors.text_muted
                    })
                    .child(text)
            }))
            .into_any_element()
    }

    fn render_choice_row(
        &self,
        id: &'static str,
        label: &'static str,
        options: &[&str],
        selected: &str,
        pick: fn(&mut ClaudeSettings, String),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let buttons: Vec<AnyElement> = options
            .iter()
            .enumerate()
            .map(|(index, &option)| {
                let value = option.to_owned();
                toggle_button((id, index), option.to_owned(), option == selected, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let value = value.clone();
                        this.apply(None, cx, |settings| pick(settings, value));
                    }))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .items_center()
            .gap_2()
            .py_1()
            .text_sm()
            .child(
                div()
                    .flex_none()
                    .w(px(LABEL_WIDTH))
                    .text_color(colors.text)
                    .child(label),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_0p5()
                    .p_0p5()
                    .rounded(radius::sm(cx))
                    .border_1()
                    .border_color(colors.border_variant)
                    .children(buttons),
            )
            .into_any_element()
    }
}

impl Render for ClaudeSettingsForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let claude_hint = claude_hint(&self.claude);
        let recheck = button("recheck-claude", "Check again", cx)
            .on_click(cx.listener(|_, _, _, cx| runs(cx).recheck_claude()))
            .into_any_element();
        let rules_path = self.settings.review_rules_path(runs(cx).argus_home());
        let rules_hint = match &self.open_error {
            Some(error) => (error.clone(), true),
            None => (
                format!("Appended to every run: {}", display_path(&rules_path)).into(),
                false,
            ),
        };
        let open_rules = button("open-review-rules", "Open in editor", cx)
            .on_click(cx.listener(|this, _, _, cx| this.open_review_rules(cx)))
            .into_any_element();
        let seconds = div()
            .text_sm()
            .text_color(cx.theme().colors.text_muted)
            .child("s")
            .into_any_element();
        let default_model = self.settings.default_model.clone();
        let default_effort = self.settings.default_effort.clone();
        let permissions = PERMISSION_CHOICES[usize::from(!self.settings.auto_approve_safe)];

        div()
            .flex()
            .flex_col()
            .child(section_header(
                "Claude",
                button("reset-claude-settings", "Reset to defaults", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.reset(cx))),
                cx,
            ))
            .child(self.render_field_row(
                Field::ClaudePath,
                "Claude binary",
                Some(recheck),
                Some(claude_hint),
                cx,
            ))
            .child(self.render_choice_row(
                "default-model",
                "Default model",
                &MODELS,
                &default_model,
                |settings, model| settings.default_model = model,
                cx,
            ))
            .child(self.render_choice_row(
                "default-effort",
                "Default effort",
                &EFFORTS,
                &default_effort,
                |settings, effort| settings.default_effort = effort,
                cx,
            ))
            .child(self.render_choice_row(
                "permissions",
                "Permissions",
                &PERMISSION_CHOICES,
                permissions,
                |settings, choice| {
                    settings.auto_approve_safe = choice == PERMISSION_CHOICES[0];
                },
                cx,
            ))
            .child(self.render_field_row(
                Field::ReviewRules,
                "Review rules",
                Some(open_rules),
                Some(rules_hint),
                cx,
            ))
            .child(
                self.render_field_row(
                    Field::CommitTimeout,
                    "Commit message timeout",
                    Some(seconds),
                    Some((
                        format!(
                            "{MIN_COMMIT_MESSAGE_TIMEOUT_S}–{MAX_COMMIT_MESSAGE_TIMEOUT_S} s; \
                         the commit dialog opens with an empty body after it"
                        )
                        .into(),
                        false,
                    )),
                    cx,
                ),
            )
    }
}

/// The run manager from the [`AppContext`] global.
fn runs(cx: &gpui::App) -> &RunManager {
    &cx.global::<AppContext>().runs
}

/// The "Claude binary" hint: the detected version and path, or why runs
/// are disabled; `true` marks an error.
fn claude_hint(status: &ClaudeStatus) -> (SharedString, bool) {
    match status {
        ClaudeStatus::Detecting => ("Looking for claude…".into(), false),
        ClaudeStatus::Unavailable(error) => (capitalize(&error.to_string()), true),
        ClaudeStatus::Detected(capabilities) => {
            if let Some(blocker) = capabilities.run_blocker() {
                return (blocker.into(), true);
            }
            let found = format!(
                "Runs use Claude Code {} at {}",
                capabilities.version_label(),
                display_path(capabilities.path())
            );
            match capabilities.warning() {
                Some(warning) => (format!("{found}. {warning}").into(), false),
                None => (found.into(), false),
            }
        }
    }
}

/// The applied value as shown in the field.
fn field_text(settings: &ClaudeSettings, field: Field) -> String {
    let path = |path: &Option<PathBuf>| {
        path.as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    };
    match field {
        Field::ClaudePath => path(&settings.claude_path),
        Field::ReviewRules => path(&settings.review_rules_path),
        Field::CommitTimeout => settings.commit_message_timeout_s.to_string(),
    }
}

fn placeholder(field: Field, cx: &gpui::App) -> SharedString {
    match field {
        Field::ClaudePath => "Auto-detect on PATH".into(),
        Field::ReviewRules => {
            display_path(&default_review_rules_path(runs(cx).argus_home())).into()
        }
        Field::CommitTimeout => "20".into(),
    }
}

fn error_text(error: &SettingError) -> SharedString {
    capitalize(&error.to_string())
}

/// Errors start lowercase so they compose; inline they read as sentences.
fn capitalize(text: &str) -> SharedString {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()).into(),
        None => SharedString::default(),
    }
}
