/**
 * @file Ini parser for clonk legacy asset files
 * @author Fridjof Milke <fridjofmilke@gmail.com>
 * @license MIT
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const idRe = /[_A-Z0-9]{4}/;

module.exports = grammar({
  name: "c4ini",

  rules: {
    source_file: $ => repeat($.section),

    section: $ => seq(
      $.section_name,
      repeat($.property),
    ),

    section_name: $ => seq(
      '[',
      $.identifier,
      ']',
    ),

    property: $ => seq(
      alias($.identifier, 'property_key'),
      '=',
      optional($.joined_value),
      //$.property_values,
    ),

    // rest of the line; immediate, so an empty value (`Goals=`) doesn't take the next line
    joined_value: $ => token.immediate(/[^\r\n]+/),

    property_values: $ => seq(
      $.property_value,
      repeat(seq(';', $.property_value)),
    ),

    property_value: $ => choice(
      $.identifier,
      $.number,
      $.id,
    ),

    id: $ => idRe,

    number: $ => choice(
      /\d+/,
      /0x[0-9a-fA-F]+/,
    ),

    identifier: $ => /[_a-zA-Z][_a-zA-Z0-9]*/,
  }
});
