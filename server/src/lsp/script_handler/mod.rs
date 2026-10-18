use tower_lsp::lsp_types::SemanticToken;
use super::{asset_handler::AssetHandler, token_types::TokenTypes};

mod highlighting;

#[derive(Debug, Clone, Default)]
pub struct ScriptHandler;

impl AssetHandler for ScriptHandler {
    fn collect_semantic_tokens(&self, tree: &tree_sitter::Tree, lut: TokenTypes, src: &str) -> Vec<SemanticToken> {
        highlighting::Highlighter::collect_tokens_with_source(tree, lut, src)
    }
}
