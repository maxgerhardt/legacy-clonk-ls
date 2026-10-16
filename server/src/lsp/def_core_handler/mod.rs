use super::{
    asset_handler::AssetHandler,
    scenario_txt_handler::definition::parse_defs,
    shared::definition::{collect_semantic_tokens, C4Ini, Definition, Defs, IniDefsProvider},
    token_types::TokenTypes,
};
use lazy_static::lazy_static;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, SemanticToken};

const UNPARSED_DEFS: &str = include_str!("./defs.csv");

lazy_static! {
    static ref DEFS: Defs<'static> = init_definitions();
}

pub struct DefCoreDefs;

impl IniDefsProvider for DefCoreDefs {
    fn get_def(section_name: &str, key: &str) -> Option<&'static Definition> {
        let map = &*DEFS;
        match map.get(section_name) {
            Some(inner_map) => inner_map.get(key),
            None => None,
        }
    }
}

fn init_definitions() -> Defs<'static> {
    parse_defs(UNPARSED_DEFS, |ctx| {
        ctx.add();
    })
}

#[derive(Debug, Clone, Default)]
pub struct DefCoreHandler;

impl AssetHandler for DefCoreHandler {
    fn collect_semantic_tokens(
        &self,
        tree: &tree_sitter::Tree,
        lut: TokenTypes,
        source: &str,
    ) -> Vec<SemanticToken> {
        collect_semantic_tokens::<DefCoreDefs>(tree, lut, source)
    }

    fn get_completions(
        &self,
        _tree: &tree_sitter::Tree,
        _source: &str,
        _cursor_pos: tower_lsp::lsp_types::Position,
    ) -> Option<Vec<CompletionItem>> {
        if let Some(section_map) = DEFS.get("HEAD") {
            Some(
                section_map
                    .keys()
                    .map(|key| CompletionItem {
                        sort_text: None,
                        kind: Some(CompletionItemKind::KEYWORD),
                        label: key.to_string(),
                        ..Default::default()
                    })
                    .collect(),
            )
        } else {
            None
        }

        //let mut already_keys = C4Ini::collect_all_keys_within_section(tree, cursor_pos, source);
        //already_keys.sort();
        //let section_name = C4Ini::get_section_from_pos(tree, cursor_pos, source);

        //if let Some(section_name) = section_name {
        //    let map = &*DEFS;
        //    if let Some(section_map) = map.get(section_name) {
        //        return Some(section_map
        //            .keys()
        //            .filter(|key| already_keys.binary_search(key).is_ok())
        //            .map(|key| super::asset_handler::CompletionItem {
        //                label: key.to_string(),
        //            })
        //            .collect());
        //    }
        //}

        //None
    }

    fn get_hover_text(&self, doc: &super::doc::Document, pos: tower_lsp::lsp_types::Position) -> Option<String> {
        C4Ini::get_hover_text::<DefCoreDefs>(doc, pos)
    }
}
