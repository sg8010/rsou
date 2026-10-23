//! 文件夹导入与设置共用的扩展名选择;Linux 与原生文件夹对话框共用。

use std::collections::BTreeSet;

use rsou_lib::import::ImportOptions;
use rsou_lib::parse::{self, FileType};
use rsou_lib::repo;

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderMode {
    Import,
    Settings,
}

pub(super) struct FolderImport {
    path: PathBuf,
    extensions: BTreeSet<String>,
    mode: FolderMode,
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
            self.ui_type(ui, kind);
        }
    }

    fn ui_type(&mut self, ui: &mut egui::Ui, kind: FileType) -> egui::Response {
        let extensions: Vec<_> = parse::supported_extensions()
            .iter()
            .copied()
            .filter(|ext| parse::file_type_of(&PathBuf::from(format!("file.{ext}"))) == Some(kind))
            .collect();
        let selected_count = extensions
            .iter()
            .filter(|ext| self.extensions.contains(**ext))
            .count();
        // 大类状态始终由扩展名推导,不另存状态,避免与单项选择或持久化设置不同步。
        let mut all_selected = selected_count == extensions.len();
        let partially_selected = selected_count > 0 && !all_selected;
        let response = ui
            .add(
                egui::Checkbox::new(
                    &mut all_selected,
                    egui::RichText::new(kind.label())
                        .strong()
                        .color(RsouApp::text_secondary()),
                )
                .indeterminate(partially_selected),
            )
            .on_hover_text("全选或取消此类格式；部分选中时点击全选。");
        if response.changed() {
            for &ext in &extensions {
                if all_selected {
                    self.extensions.insert(ext.to_owned());
                } else {
                    self.extensions.remove(ext);
                }
            }
        }
        ui.indent(kind.as_str(), |ui| {
            ui.horizontal_wrapped(|ui| {
                for ext in extensions {
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
        });
        ui.add_space(4.0);
        response
    }
}

impl RsouApp {
    pub(super) fn prepare_folder_import(&mut self, path: PathBuf) {
        self.prepare_folder_formats(path, FolderMode::Import);
    }

    pub(super) fn prepare_folder_settings(&mut self, path: PathBuf) {
        self.prepare_folder_formats(path, FolderMode::Settings);
    }

    fn prepare_folder_formats(&mut self, path: PathBuf, mode: FolderMode) {
        if self.import_active || self.maintenance_active {
            return;
        }
        let Some(conn) = &self.db else { return };
        // 设置操作精确沿用列表中的来源键,不因原文件夹离线或路径变化改写归属。
        let path = if mode == FolderMode::Import {
            dunce::canonicalize(&path).unwrap_or(path)
        } else {
            path
        };
        match repo::folder_import_extensions(conn, &path.to_string_lossy()) {
            Ok(extensions) => {
                self.folder_import = Some(FolderImport {
                    path,
                    extensions: extensions.unwrap_or_else(FolderImport::all_extensions),
                    mode,
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
        let settings = pending.mode == FolderMode::Settings;
        let mut confirm = false;
        let response =
            egui::Modal::new(egui::Id::new("rsou-folder-import-types")).show(ctx, |ui| {
                ui.set_width(540.0_f32.min(ctx.content_rect().width() - 48.0));
                Self::card_title(
                    ui,
                    if settings {
                        "文件夹设置"
                    } else {
                        "选择要索引的文件类型"
                    },
                );
                ui.add_space(8.0);
                ui.add(egui::Label::new(pending.path.display().to_string()).truncate())
                    .on_hover_text(pending.path.display().to_string());
                ui.add_space(8.0);
                if settings {
                    ui.label(egui::RichText::new("索引文件格式").strong());
                }
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
                    egui::RichText::new(if settings {
                        concat!(
                            "新增格式时执行增量更新,跳过未变化的文件。\n",
                            "取消勾选会移除此文件夹相应格式的索引及失败记录,不删除原文件。\n",
                            "单独添加的文件不受影响;重新扫描和重解析时沿用此选择。",
                        )
                    } else {
                        concat!(
                            "此选择会保存到该文件夹,重新扫描和重解析时沿用。\n",
                            "取消勾选不会移除已入库文档;单独添加文件不受此限制。",
                        )
                    })
                    .size(12.0)
                    .color(Self::text_secondary()),
                );
                if pending.extensions.is_empty() {
                    ui.label(
                        egui::RichText::new(if settings {
                            "保存后将移除此文件夹的全部文档索引,保留文件夹设置与原文件。"
                        } else {
                            "请至少选择一种文件类型。"
                        })
                        .color(Self::warning()),
                    );
                }
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    confirm = Self::primary_button(
                        ui,
                        None,
                        if settings {
                            "保存设置"
                        } else {
                            "开始索引"
                        },
                        100.0,
                        !busy && (settings || !pending.extensions.is_empty()),
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
                    sync_folder_extensions: settings,
                    ..Default::default()
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category_frame(
        ctx: &egui::Context,
        pending: &mut FolderImport,
        kind: FileType,
        events: Vec<egui::Event>,
    ) -> egui::Rect {
        let mut rect = egui::Rect::NOTHING;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ui| rect = pending.ui_type(ui, kind).rect,
        );
        output.textures_delta.clear();
        rect
    }

    fn click_category(ctx: &egui::Context, pending: &mut FolderImport, kind: FileType) {
        category_frame(ctx, pending, kind, vec![]);
        let pos = category_frame(ctx, pending, kind, vec![]).center();
        for pressed in [true, false] {
            category_frame(
                ctx,
                pending,
                kind,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
    }

    #[test]
    fn category_checkbox_toggles_only_its_extensions() {
        for kind in [
            FileType::Word,
            FileType::Excel,
            FileType::Ppt,
            FileType::Pdf,
            FileType::Text,
            FileType::Epub,
        ] {
            let ctx = egui::Context::default();
            let all = FolderImport::all_extensions();
            let mut pending = FolderImport {
                path: PathBuf::new(),
                extensions: all.clone(),
                mode: FolderMode::Import,
            };
            click_category(&ctx, &mut pending, kind);
            for ext in &all {
                let in_category =
                    parse::file_type_of(&PathBuf::from(format!("file.{ext}"))) == Some(kind);
                assert_eq!(
                    pending.extensions.contains(ext),
                    !in_category,
                    "{kind:?}: {ext}"
                );
            }
            click_category(&ctx, &mut pending, kind);
            assert_eq!(pending.extensions, all);
        }
    }

    #[test]
    fn partial_category_click_selects_all_and_tracks_individual_changes() {
        let ctx = egui::Context::default();
        let mut pending = FolderImport {
            path: PathBuf::new(),
            extensions: BTreeSet::from(["csv".to_owned(), "pdf".to_owned()]),
            mode: FolderMode::Import,
        };
        let expected: BTreeSet<_> = ["xls", "xlsx", "xlsm", "xlsb", "ods", "csv", "pdf"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        click_category(&ctx, &mut pending, FileType::Excel);
        assert_eq!(pending.extensions, expected);

        // 单项取消后,大类重新变为半选;点击应补齐而非全部取消。
        pending.extensions.remove("xlsx");
        click_category(&ctx, &mut pending, FileType::Excel);
        assert_eq!(pending.extensions, expected);
        click_category(&ctx, &mut pending, FileType::Excel);
        assert_eq!(pending.extensions, BTreeSet::from(["pdf".to_owned()]));
        click_category(&ctx, &mut pending, FileType::Pdf);
        assert!(pending.extensions.is_empty());
    }

    #[test]
    fn category_choice_round_trips_through_existing_folder_settings() {
        let ctx = egui::Context::default();
        let mut app = RsouApp::new_state(&ctx);
        app.db = Some(store::open_in_memory().unwrap());
        let root = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
        app.prepare_folder_import(root.clone());
        let pending = app.folder_import.as_mut().unwrap();
        click_category(&ctx, pending, FileType::Excel);
        let expected = pending.extensions.clone();
        repo::set_folder_import_extensions(
            app.db.as_ref().unwrap(),
            &root.to_string_lossy(),
            &expected,
        )
        .unwrap();

        app.prepare_folder_import(root);
        let pending = app.folder_import.as_mut().unwrap();
        assert_eq!(pending.extensions, expected);
        click_category(&ctx, pending, FileType::Excel);
        assert_eq!(pending.extensions, FolderImport::all_extensions());
        assert!(!app.import_active);
    }

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
