use std::fmt::Write;
use std::sync::LazyLock;

use futures_util::future::BoxFuture;
use serde::Deserialize;

use super::{Command, Ctx, Effect, Outcome, Scope, Spec};
use crate::data;
use crate::io::term;
use crate::models::preset::{self, Preset};
use crate::models::search;
use crate::models::table::{aligns, drop_order, mark, rows, source_note};
use crate::models::{self, Catalog};
use crate::text::table;
use crate::text::template::fill;

static SPEC: LazyLock<Spec> = super::spec!("models");
static TEXT: LazyLock<Text> = data::load!("commands/models.toml", "text");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Text {
    footnote: String,
    missing: String,
    none: String,
    presets: String,
}

pub struct Models;

impl Command for Models {
    fn spec(&self) -> &Spec {
        &SPEC
    }

    fn run<'a>(
        &'a self,
        cx: &'a Ctx<'a>,
        args: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Outcome>> {
        Box::pin(async move {
            let mut refresh = false;
            let mut query = String::new();
            let mut at = 0;
            while at < args.len() {
                match args[at].as_str() {
                    "--refresh" => {
                        refresh = true;
                        at += 1;
                    }
                    "--search" => {
                        let Some(words) = args.get(at + 1) else {
                            return Err(cx.usage(&SPEC));
                        };
                        query = words.clone();
                        at += 2;
                    }
                    _ => return Err(cx.usage(&SPEC)),
                }
            }
            if cx.scope == Scope::Session {
                return Ok(Effect::PickModel { refresh, query }.into());
            }
            let mut catalog = models::load(&cx.app.api, &cx.app.paths, refresh).await;
            let presets: Vec<Preset> = preset::load(&cx.app.paths)
                .into_iter()
                .filter(|p| search::matches_words(&format!("{} {}", p.id, p.description), &query))
                .collect();
            catalog.models.retain(|m| search::matches(m, &query));
            if !query.is_empty() && catalog.models.is_empty() && presets.is_empty() {
                return Ok(fill(&TEXT.none, &[("query", &query)]).into());
            }
            let shown = render(
                &catalog,
                &presets,
                &cx.app.settings.model,
                &query,
                term::out_width(),
            );
            Ok(shown.into())
        })
    }
}

/// The table laid out in `width` cells, where the list came from, then the presets by name.
fn render(
    catalog: &Catalog,
    presets: &[Preset],
    default_model: &str,
    query: &str,
    width: usize,
) -> String {
    let rows = rows(catalog, default_model);
    let mut out = table::plain_text(&rows, &aligns(), &drop_order(), width);
    let footnote = fill(&TEXT.footnote, &[("current", mark())]);
    let _ = writeln!(out, "\n{footnote} {}", source_note(catalog));
    let preset = presets.iter().any(|p| p.id == default_model);
    if query.is_empty() && !preset && !catalog.models.iter().any(|m| m.id == default_model) {
        let _ = writeln!(out, "{}", fill(&TEXT.missing, &[("model", default_model)]));
    }
    if !presets.is_empty() {
        let names: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
        let _ = writeln!(
            out,
            "{}",
            fill(&TEXT.presets, &[("names", &names.join(", "))])
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Source;

    fn catalog() -> Catalog {
        Catalog {
            models: models::snapshot(),
            source: Source::Snapshot,
            fetched_at: None,
            note: None,
        }
    }

    #[test]
    fn the_table_lines_up_with_the_default_marked() {
        let catalog = catalog();
        let presets = [preset::parsed("cheap", "[[rule]]\nmodel = \"elorus-2\"")];
        let shown = render(&catalog, &presets, "elorus-2", "", usize::MAX);
        assert!(shown.contains("Presets: cheap ("), "{shown}");
        let lines: Vec<&str> = shown.lines().take_while(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), catalog.models.len() + 1, "{shown}");
        let column = |line: &str, word: &str| line.find(word).map(|i| line[..i].chars().count());
        let model = column(lines[0], "model");
        for (line, m) in lines[1..].iter().zip(&catalog.models) {
            assert_eq!(column(line, &m.id), model, "{line}");
        }
        assert!(lines.iter().any(|l| l.starts_with("●")), "{shown}");
        assert!(!shown.contains('|'), "{shown}");
    }

    #[test]
    fn a_search_keeps_only_the_matching_models() {
        let mut catalog = catalog();
        let wanted = catalog.models[3].id.clone();
        catalog.models.retain(|m| search::matches(m, &wanted));
        assert_eq!(catalog.models.len(), 1);
        let shown = render(&catalog, &[], "elorus-2", &wanted, usize::MAX);
        assert!(shown.contains(&wanted), "{shown}");
        assert!(!shown.contains("Your default model"), "{shown}");
    }
}
