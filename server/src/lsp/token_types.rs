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
    /// if/else/for/while/in/return/break/continue: controlKeyword (keyword.control, like C#)
    pub control_kw: u32,
    /// function-local variables (declaration and use): variable
    pub variable: u32,
    /// object locals and statics of the script: property (C# fields)
    pub property: u32,
    /// %d %s %v ... in format strings: formatPlaceholder (constant.other.placeholder)
    pub placeholder: u32,
    /// \" and \\ in strings: stringEscape (constant.character.escape)
    pub escape: u32,
    /// $Name$ string table references: translation (superType macro)
    pub translation: u32,
}
