mod branch_dialog;
mod change_style;
mod conflict_dialog;
mod copy_menu;
mod design_renderer;
mod file_list;
mod git_engine_dialog;
mod git_settings_dialog;
#[cfg(test)]
mod headless;
mod history_tools_dialog;
mod paths;
mod preview;
mod preview_tabs;
mod repository_setup_dialog;
mod settings_dialog;
mod stash_dialog;
mod syntax;
mod tasks;
mod workbench;
mod workspace;
mod worktree_dialog;

pub mod theme;
pub(crate) use preview::PreviewPreferences;
pub(crate) use syntax::initialize as initialize_syntax;
pub use workspace::{Workspace, WorkspaceOptions};
