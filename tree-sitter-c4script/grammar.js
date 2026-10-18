const idRe = /[_A-Z0-9]{4}/;

/*
TODO:
- double check block vs map precedence
*/

module.exports = grammar({
    name: 'c4script',

    extras: $ => [
        $.comment,
        /\s/,
    ],

    word: $ => $.identifier,

    conflicts: $ => [
        [$._for_iterators, $.var_scope],
        [$.func_desc, $.map_access],
        // old-style function bodies end where the next "Name:" starts (needs 2 tokens of lookahead: GLR)
        [$.old_function_definition],
    ],

    rules: {

        source_file: $ => repeat($._definition),

        _definition: $ => choice(
            $.pragma_strict,
            $.appendto,
            $.function_definition,
            $.old_function_definition,
            $.var_definition,
            $.include,
        ),

        // old style: [access] Name: statements... until the next declaration (C4AulParse Parse_FuncHead, any #strict)
        old_function_definition: $ => seq(
            optional(field("visibility", $.visibility)),
            field("name", $.identifier),
            ':',
            repeat($._statement),
        ),

        function_definition: $ => seq(
            optional(field("visibility", $.visibility)),
            'func',
            optional('&'),
            field("name", $.identifier),
            $.parameter_list,
            field("block", $.block),
        ),

        var_assignment: $ => seq(
            $.identifier,
            optional(seq('=', $._expression)),
        ),

        var_definition: $ => seq(
            $.var_scope,
            optional(field('const', $.const)),
            $.var_assignment,
            repeat(seq(',', $.var_assignment)),
            ';',
        ),

        const: $ => 'const',

        var_definition_inline: $ => seq(
            $.var_scope,
            $.var_assignment,
            repeat(seq(',', $.var_assignment)),
        ),

        visibility: $ => choice(
            'private',
            'protected',
            'public',
            'global',
        ),

        parameter_list: $ => seq(
            '(',
            optional(choice($.param, '...')),
            repeat(seq(',', choice($.param, '...'))),
            ')',
        ),

        param: $ => seq(
            optional(field('type', $.identifier)),
            optional('&'),
            field('name', $.identifier),
        ),

        pragma_strict: $ => choice(
            '#strict',
            '#strict 2',
            '#strict 3',
        ),

        include: $ => seq(
            '#include',
            $.id,
        ),

        appendto: $ => seq(
            '#appendto',
            choice($.id, '*'),
            optional($.nowarn),
        ),

        nowarn: $ => seq(
            'nowarn',
        ),

        var_scope: $ => choice(
            'var',
            'local',
            'static',
        ),

        block: $ => prec(2, seq(
            '{',
            repeat($._statement),
            '}',
        )),

        _statement: $ => choice(
            $.block,
            $.func_desc,
            $.return_statement,
            $.if_statement,
            $.var_definition,
            $.while_statement,
            $.for_statement,
            $.flow_control_statement,
            $._expression_statement,
            ';',
        ),

        func_desc: $ => seq(
            '[',
            optional(repeat(/[^]]+/)),
            ']',
        ),

        _expression_statement: $ => seq(
            $._expression,
            ';',
        ),

        for_statement: $ => prec.right(seq(
            'for',
            '(',
            $._for_variations,
            ')',
            $._statement,
        )),

        _for_iterators: $ => choice(
            seq(
                optional('var'),
                $.identifier,
                'in',
                $._expression,
            ),
            seq(
                optional('var'),
                $.identifier,
                ',',
                $.identifier,
                'in',
                $._expression,
            ),
        ),

        _std_for_loop: $ => seq(
            optional(choice(
                $.var_definition_inline,
                $._expression,
            )),
            ';',
            optional($._expression),
            ';',
            optional($._expression),
        ),

        _for_variations: $ => prec(3, choice(
            $._for_iterators,
            $._std_for_loop,  
        )),

        while_statement: $ => prec.right(seq(
            'while',
            '(',
            $._expression,
            ')',
            $._statement,
        )),

        return_statement: $ => seq(
            'return',
            optional(choice(
                $._expression2,
                $._args_list,
            )),
            ';',
        ),

        flow_control_statement: $ => choice(
            seq('break', ';'),
            seq('continue', ';'),
        ),

        if_statement: $ => prec.right(seq(
            'if',
            '(',
            $._expression,
            ')',
            $._statement,
            optional(seq(
                $.else,
                $._statement,
            )),
        )),

        else: $ => 'else',

        _expression: $ => prec(2, choice(
            $.identifier,
            $.bool,
            $.nil,
            $.number,
            $.array,
            $.string,
            $.id,
            $.unary_expression,
            $.method_call,
            $.arrow_expression,
            $.binary_expression,
            $._paren_expression,
            $.builtin_constant,
            $.map,
            $.map_access,
            $.array_append,
            $.translation,
        )),

        _expression2: $ => prec(2, choice(
            $.identifier,
            $.bool,
            $.nil,
            $.number,
            $.array,
            $.string,
            $.id,
            $.unary_expression,
            $.method_call,
            $.arrow_expression,
            $.binary_expression,
            $.builtin_constant,
            $.map,
            $.map_access,
            $.array_append,
            $.translation,
        )),

        method_call: $ => seq(
            optional(seq(field('id', $.id), '::')),
            // a name that looks like an ID (ABCD()) is lexed as an id; the engine accepts it below #strict 2
            // ("stupid func label" warning), so parse it as the call's name
            field('name', choice($.identifier, alias($.id, $.identifier))),
            field('args', $.args_list),
        ),

        args_list: $ => seq(
            '(',
            optional(choice($._expression, '...')),
            repeat(seq(',', optional(choice($._expression, '...')))),
            ')',
        ),

        _args_list: $ => seq(
            '(',
            optional($._expression),
            repeat(seq(',', $._expression)),
            ')',
        ),

        // '?' = nil-safe navigation (#strict 3): obj?->Foo(), map?.key, arr?[i]
        arrow_expression: $ => seq(
            $._expression,
            optional('?'),
            '->',
            optional('~'),
            $.method_call,
        ),

        // NO_OWNER is an engine constant (an identifier): old scripts call constants like functions, NO_OWNER()
        builtin_constant: $ => choice(
            // 'DIR_LEFT',
            // 'DIR_RIGHT',
            'global',
        ),

        _paren_expression: $ => seq(
            '(',
            optional($._expression),
            ')',
        ),

        unary_expression: $ => prec(17, choice(
            prec.left(2, seq('++', $._expression)),
            seq($._expression, '++'),
            prec.left(seq('--', $._expression)),
            seq($._expression, '--'),
            prec.right(17, seq('-', $._expression)),
            prec.right(17, seq('+', $._expression)),
            prec.right(17, seq('!', $._expression)),
            prec.right(17, seq('~', $._expression)),
        )),

        binary_expression: $ => choice(
            prec.left(15, seq($._expression, '**', $._expression)),
            prec.left(14, seq($._expression, '*', $._expression)),
            prec.left(14, seq($._expression, '/', $._expression)),
            prec.left(14, seq($._expression, '%', $._expression)),
            prec.left(13, seq($._expression, '+', $._expression)),
            prec.left(13, seq($._expression, '-', $._expression)),
            prec.left(12, seq($._expression, '<<', $._expression)),
            prec.left(12, seq($._expression, '>>', $._expression)),
            prec.left(11, seq($._expression, '<=', $._expression)),
            prec.left(11, seq($._expression, '<', $._expression)),
            prec.left(11, seq($._expression, '>', $._expression)),
            prec.left(11, seq($._expression, '>=', $._expression)),
            prec.left(10, seq($._expression, '..', $._expression)),

            prec.left(9, seq($._expression, '==', $._expression)),
            prec.left(9, seq($._expression, '!=', $._expression)),
            prec.left(9, seq($._expression, 'S=', $._expression)),
            prec.left(9, seq($._expression, 'eq', $._expression)),
            prec.left(9, seq($._expression, 'ne', $._expression)),

            prec.left(8, seq($._expression, '&', $._expression)),

            prec.left(6, seq($._expression, '^', $._expression)),
            prec.left(6, seq($._expression, '|', $._expression)),

            prec.left(5, seq($._expression, '&&', $._expression)),
            prec.left(4, seq($._expression, '||', $._expression)),

            prec.left(3, seq($._expression, '??', $._expression)),

            prec.right(2, seq($._expression, '**=', $._expression)),
            prec.right(2, seq($._expression, '*=', $._expression)),
            prec.right(2, seq($._expression, '/=', $._expression)),
            prec.right(2, seq($._expression, '%=', $._expression)),
            prec.right(2, seq($._expression, '+=', $._expression)),
            prec.right(2, seq($._expression, '-=', $._expression)),
            prec.right(2, seq($._expression, '<<=', $._expression)),
            prec.right(2, seq($._expression, '>>=', $._expression)),
            prec.right(2, seq($._expression, '..=', $._expression)),
            prec.right(2, seq($._expression, '&=', $._expression)),
            prec.right(2, seq($._expression, '|=', $._expression)),
            prec.right(2, seq($._expression, '^=', $._expression)),
            prec.right(2, seq($._expression, '??=', $._expression)),
            prec.right(2, seq($._expression, '=', $._expression)),
        ),

        array: $ => prec(2, seq(
            '[',
            optional($._expression),
            repeat(seq(',', $._expression)),
            optional(','),
            ']',
        )),

        map: $ => seq(
            '{',
            optional($.map_entry),
            repeat(seq(',', $.map_entry)),
            optional(','),
            '}',
        ),

        map_entry: $ => seq(
            $.map_key,
            '=',
            $._expression,
        ),

        map_key: $ => choice(
            seq('[', $._expression, ']'),
            $.identifier,
            $.string,
        ),

        map_access: $ => choice(
            seq($._expression, optional('?'), '.', $.identifier),
            seq($._expression, optional('?'), '[', $._expression, ']'),
        ),

        // arr[] = x appends (any #strict level, C4AulParse AB_ARRAY_APPEND)
        array_append: $ => prec(2, seq($._expression, '[', ']')),

        // one token: otherwise "// ..." inside a string is lexed as a comment (extras) between '"' and the content
        string: $ => token(seq(
            '"',
            /([^"\\]|\\.)*/,
            '"',
        )),

        id: $ => idRe,

        identifier: $ => /[_a-zA-Z][_a-zA-Z0-9]*/,

        translation: $ => /\$[_a-zA-Z0-9]+\$/,

        number: $ => choice(
            /\d+/,
            /0x[0-9a-fA-F]+/,
        ),

        bool: $ => choice('true', 'false'),

        nil: $ => 'nil',

        comment: $ => token(choice(
            seq('//', /.*/),
            seq(
                '/*',
                /[^*]*\*+([^/*][^*]*\*+)*/,
                '/',
            ),
            // an unterminated /* runs to the end of the file (the engine accepts that)
            seq('/*', /([^*]|\*+[^*/])*\**/),
        )),
    },
});
