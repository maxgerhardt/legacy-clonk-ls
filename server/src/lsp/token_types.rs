#[derive(Debug, Default, Clone, Copy)]
pub struct TokenTypes {
    pub comment: u32,
    pub number: u32,
    pub string: u32,
    pub pragma_strict: u32,
    pub appendto: u32,
    pub id: u32,
    pub var_scope: u32,
    pub nil: u32,
    pub keyword: u32,
    pub parameter: u32,
    pub method: u32,
    pub parameter_type: u32,
    pub bool: u32,
    pub operator: u32,
    /// engine constants (NO_OWNER, COMD_Left ...) and the script's static consts: enumMember
    pub enum_member: u32,
    /// `this`: own type thisKeyword (scope variable.language.this)
    pub this_kw: u32,
}
