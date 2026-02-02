#include "tree_sitter/parser.h"
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

// token types from grammar.js externals
enum TokenType {
  INDENT,
  DEDENT,
  NEWLINE,
  STRING_CONTENT,
};

// scanner state: track indent level
typedef struct {
  uint16_t indent_length;
  bool at_line_start;
} Scanner;

void *tree_sitter_dagrun_external_scanner_create(void) {
  Scanner *scanner = malloc(sizeof(Scanner));
  scanner->indent_length = 0;
  scanner->at_line_start = true;
  return scanner;
}

void tree_sitter_dagrun_external_scanner_destroy(void *payload) {
  free(payload);
}

unsigned tree_sitter_dagrun_external_scanner_serialize(void *payload,
                                                        char *buffer) {
  Scanner *scanner = (Scanner *)payload;
  buffer[0] = (char)(scanner->indent_length & 0xFF);
  buffer[1] = (char)((scanner->indent_length >> 8) & 0xFF);
  buffer[2] = scanner->at_line_start ? 1 : 0;
  return 3;
}

void tree_sitter_dagrun_external_scanner_deserialize(void *payload,
                                                      const char *buffer,
                                                      unsigned length) {
  Scanner *scanner = (Scanner *)payload;
  if (length >= 3) {
    scanner->indent_length =
        (uint16_t)((unsigned char)buffer[0] | ((unsigned char)buffer[1] << 8));
    scanner->at_line_start = buffer[2] != 0;
  } else {
    scanner->indent_length = 0;
    scanner->at_line_start = true;
  }
}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }

static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

bool tree_sitter_dagrun_external_scanner_scan(void *payload, TSLexer *lexer,
                                               const bool *valid_symbols) {
  Scanner *scanner = (Scanner *)payload;

  // at end of input
  if (lexer->eof(lexer)) {
    if (valid_symbols[DEDENT] && scanner->indent_length > 0) {
      scanner->indent_length = 0;
      lexer->result_symbol = DEDENT;
      return true;
    }
    return false;
  }

  // handle newlines
  if (lexer->lookahead == '\n' || lexer->lookahead == '\r') {
    if (valid_symbols[NEWLINE]) {
      lexer->result_symbol = NEWLINE;
      advance(lexer);
      if (lexer->lookahead == '\n') {
        advance(lexer);
      }
      scanner->at_line_start = true;
      return true;
    }
  }

  // at line start, check for indent
  if (scanner->at_line_start) {
    uint16_t indent = 0;

    while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
      if (lexer->lookahead == '\t') {
        indent += 4; // tab = 4 spaces
      } else {
        indent += 1;
      }
      skip(lexer);
    }

    // blank line - don't change indent state
    if (lexer->lookahead == '\n' || lexer->lookahead == '\r') {
      return false;
    }

    scanner->at_line_start = false;

    // check for indent (entering task body)
    if (valid_symbols[INDENT] && indent > 0 && scanner->indent_length == 0) {
      scanner->indent_length = indent;
      lexer->result_symbol = INDENT;
      return true;
    }

    // check for dedent (leaving task body)
    if (valid_symbols[DEDENT] && indent < scanner->indent_length) {
      scanner->indent_length = 0;
      lexer->result_symbol = DEDENT;
      return true;
    }
  }

  return false;
}
