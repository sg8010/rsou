//! 文件夹导入前的扩展名选择;Linux 与原生文件夹对话框共用。

use std::collections::BTreeSet;

use rsou_lib::import::ImportOptions;
use rsou_lib::parse::{self, FileType};
use rsou_lib::repo;

use super::*;

pub(super) struct FolderImport {
    path: PathBuf,
    extensions: BTreeSet<String>,
}

impl FolderImport {
    fn all_extensions() -> BTreeSet<String> {
        parse::supported_extensions()
            .iter()
            .map(|ext| (*ext).to_owned())
            .collect()
    }

    fn ui_types(&mut self, ui: &mut egui::Ui) {
        for kind in [
            FileType::Word,
            FileType::Excel,
            FileType::Ppt,
            FileType::Pdf,
            FileType::Text,
            FileType::Epub,
        ] {
            ui.label(
                egui::RichText::new(kind.label())
                    .strong()
                    .color(RsouApp::text_secondary()),
            );
            ui.horizontal_wrapped(|ui| {
                for &ext in parse::supported_extensions() {
                    if parse::file_type_of(&PathBuf::from(format!("file.{ext}"))) != Some(kind) {
                        continue;
                    }
                    let mut selected = self.extensions.contains(ext);
                    if ui
                        .checkbox(&mut selected, ext.to_ascii_uppercase())
                        .changed()
                    {
                        if selected {
                            self.extensions.insert(ext.to_owned());
                        } else {
                            self.extensions.remove(ext);
                        }
                    }
                }
            });
            ui.add_space(4.0);
        }
    }
}

impl RsouApp {
    pub(super) fn prepare_folder_import(&mut self, path: PathBuf) {
        let Some(conn) = &self.db else { return };
        let path = dunce::canonicalize(&path).unwrap_or(path);
        match repo::folder_import_extensions(conn, &path.to_string_lossy()) {
            Ok(extensions) => {
                self.folder_import = Some(FolderImport {
                    path,
                    extensions: extensions.unwrap_or_else(FolderImport::all_extensions),
                });
            }
            Err(error) => {
                self.library_notice = Some(format!("读取文件夹索引类型失败: {error:#}"));
            }
        }
    }

    pub(super) fn ui_folder_import(&mut self, ctx: &egui::Context) {
        let Some(pending) = &mut self.folder_import else {
            return;
        };
        let busy = self.import_active || self.maintenance_active || self.db.is_none();
        let mut confirm = false;
        let response =
            egui::Modal::new(egui::Id::new("rsou-folder-import-types")).show(ctx, |ui| {
                ui.set_width(540.0_f32.min(ctx.content_rect().width() - 48.0));
                Self::card_title(ui, "选择要索引的文件类型");
                ui.add_space(8.0);
                ui.add(egui::Label::new(pending.path.display().to_string()).truncate())
                    .on_hover_text(pending.path.display().to_string());
                ui.add_space(8.0);
                ui.label("递归扫描此文件夹及子文件夹,只导入勾选的格式。");
                ui.horizontal(|ui| {
                    if Self::link_button(ui, "全选").clicked() {
                        pending.extensions = FolderImport::all_extensions();
                    }
                    if Self::link_button(ui, "清空选择").clicked() {
                        pending.extensions.clear();
                    }
                    ui.label(
                        egui::RichText::new(format!("已选 {} 种格式", pending.extensions.len()))
                            .color(Self::text_muted()),
                    );
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 300.0).clamp(100.0, 400.0))
                    .show(ui, |ui| pending.ui_types(ui));
                ui.separator();
                ui.label(
                    egui::RichText::new(concat!(
                        "此选择会保存到该文件夹,重新扫描和重解析时沿用。\n",
                        "取消勾选不会移除已入库文档;单独添加文件不受此限制。",
                    ))
                    .size(12.0)
                    .color(Self::text_secondary()),
                );
                if pending.extensions.is_empty() {
                    ui.label(
                        egui::RichText::new("请至少选择一种文件类型。").color(Self::warning()),
                    );
                }
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    confirm = Self::primary_button(
                        ui,
                        None,
                        "开始索引",
                        100.0,
                        !busy && !pending.extensions.is_empty(),
                    )
                    .clicked();
                    if Self::secondary_button(ui, None, "取消", 80.0, true).clicked() {
                        ui.close();
                    }
                });
            });
        if response.should_close() {
            self.folder_import = None;
        } else if confirm {
            let pending = self.folder_import.take().unwrap();
            self.start_import_with_options(
                ctx,
                vec![pending.path],
                ImportOptions {
                    extensions: Some(pending.extensions),
                    ..Default::default()
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_folder_defaults_to_all_formats_without_starting_import() {
        let mut app = RsouApp::new_state(&egui::Context::default());
        app.db = Some(store::open_in_memory().unwrap());
        app.prepare_folder_import(PathBuf::from("/new-folder"));
        assert_eq!(
            app.folder_import.as_ref().unwrap().extensions,
            FolderImport::all_extensions()
        );
        assert!(!app.import_active);
        assert!(
            repo::folder_import_extensions(app.db.as_ref().unwrap(), "/new-folder")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn dialog_loads_saved_selection_and_escape_discards_edits() {
        let ctx = egui::Context::default();
        let mut app = RsouApp::new_state(&ctx);
        app.db = Some(store::open_in_memory().unwrap());
        let root = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let selected = BTreeSet::from(["pdf".to_owned()]);
        repo::set_folder_import_extensions(
            app.db.as_ref().unwrap(),
            &root.to_string_lossy(),
            &selected,
        )
        .unwrap();
        app.prepare_folder_import(root.join("."));
        assert_eq!(app.folder_import.as_ref().unwrap().extensions, selected);
        app.folder_import.as_mut().unwrap().extensions.clear();
        let mut output = ctx.run_ui(Default::default(), |ui| app.ui_folder_import(ui.ctx()));
        output.textures_delta.clear();
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui_folder_import(ui.ctx()));
        output.textures_delta.clear();
        assert!(app.folder_import.is_none());
        assert!(!app.import_active);
        assert_eq!(
            repo::folder_import_extensions(app.db.as_ref().unwrap(), &root.to_string_lossy())
                .unwrap(),
            Some(selected)
        );
    }
}
