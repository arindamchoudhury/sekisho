//! Kilo CLI, a fork of OpenCode, from its source at `Kilo-Org/kilocode`
//! `698ff4e` (2026-11-27, npm `@kilocode/cli` 7.8.40): OpenCode's shapes
//! in `kilo.json` and `.kilo/`, written by [`super::opencode`]. A project's
//! config is untrusted there: one holding `{env:}` loads as empty, so the
//! providers go in the global `~/.config/kilo/kilo.json` (D192) and the
//! project file names its model by their ID.

use sekisho_catalog::delivery::Delivery;

use super::opencode::{self, Flavor};
use super::{
    Adapter, AdapterError, Layering, RenderContext, Rendered, RunSpec, Tool, UnmanagedItem,
};
use crate::install::Scope;

const KILO: Flavor = Flavor {
    tool: Tool::KiloCli,
    program: "kilo",
    dir: ".kilo",
    base: ".kilo/",
    project_file: ".kilo/kilo.json",
    file: "kilo.json",
    global_providers: true,
};

/// Kilo CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct KiloCli;

impl Adapter for KiloCli {
    fn tool(&self) -> Tool {
        Tool::KiloCli
    }

    fn detect(&self, scope: &Scope) -> bool {
        opencode::detect(KILO, scope)
    }

    fn render(&self, d: &Delivery, ctx: &RenderContext<'_>) -> Result<Rendered, AdapterError> {
        opencode::render(KILO, d, ctx)
    }

    fn unmanaged(
        &self,
        _scope: &Scope,
        _locks: &[&crate::install::lock::Lock],
    ) -> Vec<UnmanagedItem> {
        // Task 10 (`sekisho status`) reads the tool's config for these.
        Vec::new()
    }

    fn layering(&self) -> Layering {
        opencode::layering()
    }

    fn run_spec(&self, d: &Delivery) -> Option<RunSpec> {
        opencode::run_spec(KILO, d)
    }
}
