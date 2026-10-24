//! 「バージョン情報」の窓。名前・版・著作権・ライセンス・Issue の画面へのリンクと、
//! 同梱のサードパーティのライセンス一覧を開くボタンを出す(vw の「vw について」、DicoAmplio と同じ形)。

use std::path::PathBuf;

use eframe::egui;

pub const COPYRIGHT: &str = "Copyright (c) 2026 xex (xex-ken-xex)";
pub const AUTHOR: &str = "xex (xex-ken-xex)";
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// 不具合の報告と要望。
pub const ISSUES: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");
pub const LICENSE: &str = env!("CARGO_PKG_LICENSE");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 配布物で実行ファイルと同じフォルダに置かれる、依存クレートのライセンス一覧。
const THIRD_PARTY: &str = "THIRD-PARTY-LICENSES.txt";

#[derive(Default)]
pub struct About {
    pub open: bool,
}

impl About {
    /// 開いていれば描く。一覧を開けなかったときは、その理由を返す(ログに出す)。
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<String> {
        if !self.open {
            return None;
        }
        let mut error = None;
        let mut open = self.open;
        egui::Window::new("バージョン情報")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.heading(format!("folder2iso {VERSION}"));
                ui.label("フォルダから ISO イメージを作るツール");
                ui.add_space(6.0);
                ui.label(COPYRIGHT);
                ui.horizontal(|ui| {
                    ui.label("作者");
                    ui.label(AUTHOR);
                });
                ui.hyperlink(REPOSITORY);
                ui.hyperlink_to("不具合の報告・要望…", ISSUES);
                ui.horizontal(|ui| {
                    ui.label("ライセンス");
                    ui.label(LICENSE);
                });
                ui.add_space(6.0);
                let path = third_party_path();
                let exists = path.as_ref().is_some_and(|p| p.exists());
                let button = ui
                    .add_enabled(exists, egui::Button::new("サードパーティのライセンス…"))
                    .on_disabled_hover_text(
                        "THIRD-PARTY-LICENSES.txt が実行ファイルの隣にない(配布の zip に入っている)",
                    );
                if button.clicked() {
                    if let Some(path) = path {
                        if let Err(e) = vw_launch::open_file(&path) {
                            error = Some(format!("{}: {e}", path.display()));
                        }
                    }
                }
            });
        self.open = open;
        error
    }
}

fn third_party_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(THIRD_PARTY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_shows_the_workspace_license_version_and_repository() {
        assert_eq!(LICENSE, "MIT OR Apache-2.0");
        assert_eq!(REPOSITORY, "https://github.com/xex-ken-xex/folder2iso");
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn issue_link_opens_the_issue_list() {
        assert_eq!(ISSUES, "https://github.com/xex-ken-xex/folder2iso/issues");
    }
}
