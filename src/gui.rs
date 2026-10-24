//! 画面。見た目は vw と同じ(`vw-ui`)、フォルダやファイルを選ぶのは vw のツリー(`vw-tree` の `picker`)。
//! OS のダイアログを使わないのは vw・DicoAmplio と同じ理由(Linux の依存が重く、見た目も揃わない)。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ThemePreference};
use vw_tree::picker::{self, Picker};

use crate::iso::{self, Summary};

const WARN: Color32 = Color32::from_rgb(0xe0, 0xa0, 0x40);
const ERROR: Color32 = Color32::from_rgb(0xe0, 0x60, 0x60);
const OK: Color32 = Color32::from_rgb(0x6a, 0xc0, 0x7a);

pub fn run(source: Option<PathBuf>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("folder2iso")
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    vw_renderer::run_native(
        "folder2iso",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, source)))),
    )
    .map_err(|e| e.to_string())
}

struct Job {
    progress: Arc<iso::Progress>,
    started: Instant,
    output: PathBuf,
    result: Receiver<Result<iso::Report, String>>,
}

struct Scan {
    source: String,
    exclude_hidden: bool,
    result: Receiver<Result<Summary, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    Source,
    Output,
}

struct App {
    source: String,
    output: String,
    /// 出力の欄を自分で書いたか。書いていなければ、元のフォルダに合わせて変える
    output_edited: bool,
    label: String,
    exclude_hidden: bool,
    overwrite: bool,
    media: iso::Media,
    theme: ThemePreference,

    /// 数えた元のフォルダと、その結果
    summary: Option<(String, bool, Result<Summary, String>)>,
    scan: Option<Scan>,
    job: Option<Job>,
    /// 最後に作った ISO(「フォルダを開く」で見せる)
    last_output: Option<PathBuf>,
    log: Vec<String>,
    picker: Option<(PickTarget, Picker)>,
    about: crate::about::About,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, source: Option<PathBuf>) -> Self {
        crate::fonts::load_cjk_in_background(&cc.egui_ctx);
        let theme = ThemePreference::System;
        vw_ui::theme::apply(&cc.egui_ctx, theme, true, 1.0);
        let mut app = App {
            source: String::new(),
            output: String::new(),
            output_edited: false,
            label: String::new(),
            exclude_hidden: false,
            overwrite: false,
            media: iso::Media::default(),
            theme,
            summary: None,
            scan: None,
            job: None,
            last_output: None,
            log: vec!["起動した。元のフォルダは、窓にフォルダをドロップしても指定できる".into()],
            picker: None,
            about: Default::default(),
        };
        if let Some(s) = source {
            // 相対パスのままだと、出力の既定(フォルダの隣)が作業フォルダ基準の短い名前になって分かりにくい
            let s = std::path::absolute(&s).unwrap_or(s);
            app.set_source(&s.display().to_string());
        }
        app
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.log.push(msg.into());
    }

    fn set_source(&mut self, text: &str) {
        self.source = text.to_string();
        if !self.output_edited && !text.trim().is_empty() {
            self.output = iso::default_output(Path::new(text.trim()))
                .display()
                .to_string();
        }
    }

    /// 元のフォルダか「隠しファイルを入れない」が変わっていれば、裏で数え直す。
    fn refresh_scan(&mut self, ctx: &egui::Context) {
        let source = self.source.trim().to_string();
        let current = |s: &str, h: bool| s == source && h == self.exclude_hidden;
        if source.is_empty()
            || self
                .scan
                .as_ref()
                .is_some_and(|s| current(&s.source, s.exclude_hidden))
            || self
                .summary
                .as_ref()
                .is_some_and(|(s, h, _)| current(s, *h))
        {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (path, hidden, ctx) = (PathBuf::from(&source), self.exclude_hidden, ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(iso::scan(&path, hidden));
            ctx.request_repaint();
        });
        self.scan = Some(Scan {
            source,
            exclude_hidden: self.exclude_hidden,
            result: rx,
        });
    }

    fn poll_scan(&mut self) {
        let Some(scan) = &self.scan else {
            return;
        };
        if let Ok(r) = scan.result.try_recv() {
            let scan = self.scan.take().expect("checked above");
            self.summary = Some((scan.source, scan.exclude_hidden, r));
        }
    }

    fn current_summary(&self) -> Option<&Result<Summary, String>> {
        self.summary
            .as_ref()
            .filter(|(s, h, _)| s == self.source.trim() && *h == self.exclude_hidden)
            .map(|(_, _, r)| r)
    }

    /// 作れない理由。なければ None。
    fn blocking_problem(&self) -> Option<String> {
        let source = self.source.trim();
        if source.is_empty() {
            return Some("元のフォルダを選んでください".into());
        }
        if !Path::new(source).is_dir() {
            return Some("元のフォルダが見つかりません".into());
        }
        let output = self.output.trim();
        if output.is_empty() {
            return Some("出力する ISO ファイルを決めてください".into());
        }
        if Path::new(output).is_dir() {
            return Some("出力先がフォルダになっています。ファイル名まで入れてください".into());
        }
        if !self.overwrite && Path::new(output).exists() {
            return Some(
                "出力ファイルが既にあります（上書きするなら「既にあれば上書きする」）".into(),
            );
        }
        if cfg!(windows) && crate::imapi::script_path().is_none() {
            return Some(format!(
                "{} が見つかりません。folder2iso.exe と同じフォルダに置いてください",
                crate::imapi::SCRIPT_NAME
            ));
        }
        match self.current_summary() {
            Some(Err(e)) => Some(e.clone()),
            // Windows は IMAPI2 が UDF で入れるので止めない
            Some(Ok(s)) if !cfg!(windows) && !s.too_big.is_empty() => Some(format!(
                "4GiB 以上のファイルが {} 個あり、ISO9660/Joliet には入れられません",
                s.too_big.len()
            )),
            _ => None,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        if self.job.is_some() || self.blocking_problem().is_some() {
            return;
        }
        let opts = iso::Options {
            source: PathBuf::from(self.source.trim()),
            output: PathBuf::from(self.output.trim()),
            label: self.label.clone(),
            overwrite: self.overwrite,
            exclude_hidden: self.exclude_hidden,
            media: self.media,
        };
        self.note(format!(
            "作成を始めた: {} → {}",
            opts.source.display(),
            opts.output.display()
        ));
        let progress = Arc::new(iso::Progress::default());
        let (tx, rx) = mpsc::channel();
        let (p, ctx) = (Arc::clone(&progress), ctx.clone());
        let output = opts.output.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::build(&opts, &p));
            ctx.request_repaint();
        });
        self.job = Some(Job {
            progress,
            started: Instant::now(),
            output,
            result: rx,
        });
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        let Ok(result) = job.result.try_recv() else {
            return;
        };
        let job = self.job.take().expect("checked above");
        match result {
            Ok(report) => {
                for w in &report.warnings {
                    self.log.push(format!("⚠ {w}"));
                }
                self.note(format!(
                    "作成した: {}（{} ファイル、{} フォルダ、{}、{:.1} 秒）",
                    job.output.display(),
                    report.summary.files,
                    report.summary.dirs,
                    size_text(report.image_bytes),
                    job.started.elapsed().as_secs_f64()
                ));
                self.last_output = Some(job.output);
            }
            Err(e) => self.note(format!("エラー: {e}")),
        }
    }

    // ---- メニューと帯 ----------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut start = false;
        vw_ui::menu::bar(ui, |ui| {
            vw_ui::menu::bar_menu(ui, "ファイル", |ui| {
                if ui
                    .add_enabled(
                        self.job.is_none() && self.blocking_problem().is_none(),
                        vw_ui::menu::item(false, "ISO を作成", "Ctrl+Enter"),
                    )
                    .clicked()
                {
                    ui.close();
                    start = true;
                }
                let last = self.last_output.clone();
                if ui
                    .add_enabled(
                        last.is_some(),
                        vw_ui::menu::item(false, "作った ISO のフォルダを開く", ""),
                    )
                    .clicked()
                {
                    ui.close();
                    self.open_output_folder();
                }
                ui.separator();
                if ui.add(vw_ui::menu::item(false, "終了", "")).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            vw_ui::menu::bar_menu(ui, "表示", |ui| {
                for (pref, label) in [
                    (ThemePreference::System, "テーマ: システムに合わせる"),
                    (ThemePreference::Dark, "テーマ: ダーク"),
                    (ThemePreference::Light, "テーマ: ライト"),
                ] {
                    if ui
                        .add(vw_ui::menu::item(self.theme == pref, label, ""))
                        .clicked()
                    {
                        ui.close();
                        self.theme = pref;
                        vw_ui::theme::apply(&ctx, pref, true, 1.0);
                    }
                }
            });
            vw_ui::menu::bar_menu(ui, "ヘルプ", |ui| {
                if ui
                    .add(vw_ui::menu::item(false, "バージョン情報", ""))
                    .clicked()
                {
                    ui.close();
                    self.about.open = true;
                }
            });
        });
        let run_key = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter));
        if start || run_key {
            self.start(&ctx);
        }
    }

    /// メニューの下の帯。作成のボタンと、いま何が起きるかの一行。
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut start = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            match &self.job {
                Some(j) => {
                    cancel = ui.button("■ 中断").clicked();
                    let total = j.progress.total.load(Ordering::Relaxed);
                    let done = j.progress.done.load(Ordering::Relaxed);
                    let f = if total == 0 {
                        0.0
                    } else {
                        done as f32 / total as f32
                    };
                    ui.add(egui::ProgressBar::new(f).desired_width(260.0).text(format!(
                        "{} / {}",
                        size_text(done),
                        size_text(total)
                    )));
                    let secs = j.started.elapsed().as_secs_f64();
                    if done == 0 {
                        // 書き出しの前(数える・IMAPI2 がファイルを集める)は量が出ないので、段階を出す
                        let stage = j.progress.stage();
                        if !stage.is_empty() {
                            ui.weak(format!("{stage}…"));
                        }
                    } else if secs > 0.5 {
                        let rate = done as f64 / secs;
                        let left = total.saturating_sub(done) as f64 / rate;
                        ui.weak(format!("{}/秒、残り {left:.0} 秒", size_text(rate as u64)));
                    }
                }
                None => {
                    let problem = self.blocking_problem();
                    let button = egui::Button::new(RichText::new("▶ ISO を作成").strong());
                    start = ui.add_enabled(problem.is_none(), button).clicked();
                    match problem {
                        Some(p) => {
                            ui.colored_label(WARN, format!("⚠ {p}"));
                        }
                        None => {
                            ui.weak(format!("→ {}", self.output.trim()));
                        }
                    }
                }
            }
        });
        if cancel {
            if let Some(j) = &self.job {
                j.progress.cancel.store(true, Ordering::Relaxed);
            }
        }
        if start {
            self.start(&ctx);
        }
    }

    // ---- 設定 ----------------------------------------------------------------------------

    /// 設定の節の見出し。中身の字より少し大きい字と、下の細線で区切る(DicoAmplio と同じ)。
    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(10.0);
        ui.label(RichText::new(title).strong());
        ui.add_space(2.0);
        ui.separator();
        ui.add_space(4.0);
    }

    /// パスの欄。入力欄(残りの幅いっぱい)と「参照」。参照を押したら true。
    fn path_row(ui: &mut egui::Ui, value: &mut String, hint: &str) -> (egui::Response, bool) {
        let mut browse = false;
        let r = ui
            .horizontal(|ui| {
                let browse_w = 52.0;
                let r = ui.add(
                    egui::TextEdit::singleline(value)
                        .desired_width(ui.available_width() - browse_w)
                        .hint_text(hint),
                );
                browse = ui.button("参照").clicked();
                r
            })
            .inner;
        (r, browse)
    }

    fn picker_labels() -> picker::Labels {
        picker::Labels {
            choose: "選ぶ".into(),
            cancel: "やめる".into(),
            name: "名前".into(),
            nothing: "(フォルダを選んでいない)".into(),
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let enabled = self.job.is_none();
        ui.add_enabled_ui(enabled, |ui| {
            Self::section(ui, "元のフォルダ");
            let mut source = self.source.clone();
            let (r, browse) = Self::path_row(ui, &mut source, "ISO に入れるフォルダ");
            if r.changed() {
                self.set_source(&source);
            }
            if browse {
                let start = self.source.trim();
                self.picker = Some((
                    PickTarget::Source,
                    Picker::folder(
                        "ISO に入れるフォルダを選ぶ",
                        (!start.is_empty()).then_some(Path::new(start)),
                        Self::picker_labels(),
                    ),
                ));
            }
            self.summary_ui(ui);

            Self::section(ui, "出力する ISO ファイル");
            let (r, browse) = Self::path_row(ui, &mut self.output, "保存する .iso のパス");
            if r.changed() {
                self.output_edited = true;
            }
            if browse {
                let current = PathBuf::from(self.output.trim());
                let name = current
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image.iso".into());
                let near = current.parent().map(Path::to_path_buf);
                self.picker = Some((
                    PickTarget::Output,
                    Picker::new(
                        "ISO の保存先を選ぶ",
                        picker::Mode::SaveFile {
                            extensions: vec!["iso".into()],
                            name,
                        },
                        near.as_deref().filter(|p| !p.as_os_str().is_empty()),
                        Self::picker_labels(),
                    ),
                ));
            }
            ui.checkbox(&mut self.overwrite, "既にあれば上書きする");

            Self::section(ui, "オプション");
            ui.horizontal(|ui| {
                ui.label("ボリュームラベル");
                let hint = if self.source.trim().is_empty() {
                    "(フォルダ名)".to_string()
                } else {
                    iso::folder_name(Path::new(self.source.trim()))
                };
                ui.add(
                    egui::TextEdit::singleline(&mut self.label)
                        .desired_width(ui.available_width())
                        .hint_text(hint),
                )
                .on_hover_text("エクスプローラでドライブ名の横に出る名前。空ならフォルダ名");
            });
            ui.horizontal(|ui| {
                ui.label("メディア");
                vw_ui::menu::combo("media")
                    .selected_text(self.media.label())
                    .show_ui(ui, |ui| {
                        for m in iso::Media::ALL {
                            ui.selectable_value(&mut self.media, m, m.label());
                        }
                    });
            })
            .response
            .on_hover_text(
                "焼く予定のメディア。入りきらなければ作る前に止める。焼かないなら「上限なし」",
            );
            ui.checkbox(&mut self.exclude_hidden, "隠しファイル・フォルダを入れない")
                .on_hover_text("Windows の隠し属性のもの、名前が \".\" で始まるもの");
            ui.add_space(6.0);
            ui.weak(if cfg!(windows) {
                "形式: ISO9660 + Joliet + UDF（Windows 標準の IMAPI2 で作る）。\
                 4GiB 以上のファイルがあれば UDF だけで作ります。"
            } else {
                "形式: ISO9660 + Joliet（日本語・長い名前は 64 文字まで）。\
                 4GiB 以上のファイルは入れられません。"
            });
        });
    }

    fn summary_ui(&self, ui: &mut egui::Ui) {
        if self.source.trim().is_empty() {
            return;
        }
        if self.scan.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("中身を数えている…");
            });
            return;
        }
        match self.current_summary() {
            Some(Ok(s)) => {
                let blocked = !cfg!(windows) && !s.too_big.is_empty();
                ui.colored_label(
                    if blocked { WARN } else { OK },
                    format!(
                        "{} ファイル、{} フォルダ、{}",
                        s.files,
                        s.dirs,
                        size_text(s.bytes)
                    ),
                );
                if blocked {
                    for p in s.too_big.iter().take(5) {
                        ui.colored_label(WARN, format!("4GiB 以上: {}", p.display()));
                    }
                } else if !s.too_big.is_empty() {
                    ui.weak(format!(
                        "4GiB 以上のファイルが {} 個あるので、UDF だけで作ります",
                        s.too_big.len()
                    ));
                }
            }
            Some(Err(e)) => {
                ui.colored_label(ERROR, e);
            }
            None => {}
        }
    }

    fn picker_ui(&mut self, ctx: &egui::Context) {
        let Some((target, picker)) = &mut self.picker else {
            return;
        };
        let target = *target;
        let chosen = match picker.show(ctx) {
            picker::Outcome::Open => return,
            picker::Outcome::Cancelled => None,
            picker::Outcome::Chosen(p) => Some(p),
        };
        self.picker = None;
        let Some(p) = chosen else {
            return;
        };
        let text = p.display().to_string();
        match target {
            PickTarget::Source => {
                self.set_source(&text);
                self.note(format!("元のフォルダ: {text}"));
            }
            PickTarget::Output => {
                self.output = text;
                self.output_edited = true;
            }
        }
    }

    fn take_dropped_folder(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let dropped: Option<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        let Some(p) = dropped else {
            return;
        };
        let dir = if p.is_dir() {
            p
        } else {
            p.parent().map(Path::to_path_buf).unwrap_or(p)
        };
        let text = dir.display().to_string();
        self.set_source(&text);
        self.note(format!("元のフォルダ: {text}"));
    }

    fn open_output_folder(&mut self) {
        let Some(dir) = self
            .last_output
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
        else {
            return;
        };
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        if let Err(e) = vw_launch::open_folder(&dir) {
            self.note(format!(
                "エラー: フォルダを開けない: {}: {e}",
                dir.display()
            ));
        }
    }

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            vw_ui::panel::header(ui, "ログ");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("消す").clicked() {
                    self.log.clear();
                }
            });
        });
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                for l in &self.log {
                    let text = RichText::new(l).monospace();
                    if l.starts_with("エラー") {
                        ui.colored_label(ERROR, text);
                    } else if l.starts_with('⚠') {
                        ui.colored_label(WARN, text);
                    } else {
                        ui.label(text);
                    }
                }
            });
    }
}

/// 人が読む大きさ(1024 区切り)。
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job();
        self.poll_scan();
        self.take_dropped_folder(&ctx);
        self.refresh_scan(&ctx);
        if self.job.is_some() || self.scan.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // メニューと道具の帯は窓の地の色、パネルはその一段上にする(vw と同じ)
        let chrome = ui.visuals().extreme_bg_color;
        egui::Panel::top("menu")
            .frame(egui::Frame::side_top_panel(ui.style()).fill(chrome))
            .show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .fill(chrome)
                    .inner_margin(egui::Margin::symmetric(6, 6)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(140.0)
            .show(ui, |ui| self.log_ui(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| self.settings_ui(ui));
        });
        self.picker_ui(&ctx);
        if let Some(e) = self.about.ui(&ctx) {
            self.note(format!("エラー: ライセンスの一覧を開けない: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_shown_in_binary_units() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(1536), "1.5 KiB");
        assert_eq!(size_text(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
