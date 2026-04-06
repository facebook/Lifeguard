#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# pyre-unsafe
"""
Script to merge effects from all overloads into the first overload in stub files.
All subsequent overload bodies are replaced with '...'.
- Single effects are kept inline with the signature.
- Inline comments are preserved with their original overload signatures.
- Docstrings are preserved in their original positions.
"""

from typing import cast, Sequence

import libcst as cst
from libcst import matchers as m


class OverloadMerger(cst.CSTTransformer):
    """
    Merges effects from all overloads of the same method into the first overload.
    Subsequent overload bodies are replaced with '...'.
    """

    # NOTE: Most of the methods in here don't depend on `self`, they are left
    # as methods simply for code organisation.

    def leave_Module(
        self, original_node: cst.Module, updated_node: cst.Module
    ) -> cst.Module:
        """Process and merge overload groups at module level."""
        new_body = self._process_statements(updated_node.body)
        return updated_node.with_changes(body=new_body)

    def leave_ClassDef(
        self, original_node: cst.ClassDef, updated_node: cst.ClassDef
    ) -> cst.ClassDef:
        """Process and merge overload groups within a class."""
        # Cast needed because libcst stubs are conservative about body.body type
        body_stmts = cast(Sequence[cst.BaseStatement], updated_node.body.body)
        new_body = self._process_statements(body_stmts)
        new_suite = updated_node.body.with_changes(body=new_body)
        return updated_node.with_changes(body=new_suite)

    def _process_statements(
        self, statements: Sequence[cst.BaseStatement]
    ) -> Sequence[cst.BaseStatement]:
        """Process a sequence of statements, merging overload groups."""
        out = []
        i = 0

        while i < len(statements):
            stmt = statements[i]

            # Check if this is an overloaded function
            if self._is_overload(stmt):
                # Collect all consecutive overloads with the same name
                # pyre-ignore[16]: stmt is narrowed by _is_overload
                func_name = stmt.name.value
                overload_group = [stmt]
                j = i + 1

                while j < len(statements):
                    next_stmt = statements[j]
                    if (
                        isinstance(next_stmt, cst.FunctionDef)
                        and self._is_overload(next_stmt)
                        and next_stmt.name.value == func_name
                    ):
                        overload_group.append(next_stmt)
                        j += 1
                    else:
                        break

                # Merge the overload group
                # pyre-ignore[6]: overload_group is narrowed here
                merged = self._merge_overload_group(overload_group)
                out.extend(merged)
                i = j
            else:
                out.append(stmt)
                i += 1

        return out

    def _is_overload(self, node: cst.BaseStatement) -> bool:
        """Check if a statement is a function with the @overload decorator."""
        return isinstance(node, cst.FunctionDef) and any(
            m.matches(decorator.decorator, m.Name("overload"))
            for decorator in node.decorators
        )

    def _is_ellipsis_expr(self, stmt: cst.BaseSmallStatement) -> bool:
        """Check if a statement is just an ellipsis."""
        return isinstance(stmt, cst.Expr) and isinstance(stmt.value, cst.Ellipsis)

    def _get_trailing_whitespace(
        self, body: cst.BaseSuite
    ) -> cst.TrailingWhitespace | None:
        """Extract trailing whitespace (includes comments) from a function body."""
        if isinstance(body, cst.SimpleStatementSuite):
            return body.trailing_whitespace
        return None

    def _get_header(self, body: cst.BaseSuite) -> cst.TrailingWhitespace:
        """Extract header from a function body."""
        if isinstance(body, cst.IndentedBlock):
            return body.header
        return cst.TrailingWhitespace()

    def _get_footer(self, body: cst.BaseSuite) -> Sequence[cst.EmptyLine]:
        """Extract footer from a function body."""
        if isinstance(body, cst.IndentedBlock):
            return body.footer
        return []

    def _merge_overload_group(
        self, overloads: list[cst.FunctionDef]
    ) -> list[cst.FunctionDef]:
        """Merge a group of overloads, returning the transformed list."""
        if len(overloads) < 2:
            return overloads

        # Collect all effects from all overloads
        all_effects = []
        seen_effects = set()

        for overload in overloads:
            effects = self._extract_effects(overload.body)
            for effect in effects:
                effect_code = cst.Module([]).code_for_node(effect)
                if effect_code not in seen_effects:
                    seen_effects.add(effect_code)
                    all_effects.append(effect)

        # Create new body for first overload with merged effects
        first = overloads[0]
        docstring = self._extract_docstring(first.body)
        trailing_ws = self._get_trailing_whitespace(first.body)
        header = self._get_header(first.body)
        footer = self._get_footer(first.body)
        new_first_body = self._create_merged_body(
            all_effects,
            docstring,
            header,
            footer,
            trailing_ws,
        )
        out = [first.with_changes(body=new_first_body)]

        # Transform subsequent overloads to have '...' body
        for overload in overloads[1:]:
            body = overload.body
            docstring = self._extract_docstring(body)
            trailing_ws = self._get_trailing_whitespace(body)
            header = self._get_header(body)
            footer = self._get_footer(body)
            new_body = self._create_ellipsis_body(
                docstring,
                header,
                footer,
                trailing_ws,
            )
            out.append(overload.with_changes(body=new_body))

        return out

    def _extract_docstring(self, body: cst.BaseSuite) -> cst.SimpleStatementLine | None:
        """Extract the docstring from a function body, if present."""
        if not isinstance(body, cst.IndentedBlock) or not body.body:
            return None

        first_stmt = body.body[0]
        if isinstance(first_stmt, cst.SimpleStatementLine):
            if len(first_stmt.body) == 1 and isinstance(first_stmt.body[0], cst.Expr):
                expr = first_stmt.body[0]
                # pyre-ignore[16]: expr is narrowed here
                if isinstance(expr.value, (cst.SimpleString, cst.ConcatenatedString)):
                    return first_stmt

        return None

    def _extract_effects(self, body: cst.BaseSuite) -> list[cst.BaseStatement]:
        """Extract effect statements (non-docstring, non-ellipsis) from a function body."""
        if isinstance(body, cst.SimpleStatementSuite):
            # Inline body - check if it's an ellipsis
            effects: list[cst.BaseStatement] = []
            for stmt in body.body:
                if not self._is_ellipsis_expr(stmt):
                    # Preserve trailing_whitespace (includes comments)
                    effects.append(
                        cst.SimpleStatementLine(
                            body=[stmt], trailing_whitespace=body.trailing_whitespace
                        )
                    )
            return effects

        if not isinstance(body, cst.IndentedBlock):
            return []

        # Skip docstring if present
        skip_first = self._extract_docstring(body) is not None
        effects = []

        for i, stmt in enumerate(body.body):
            if skip_first and i == 0:
                continue

            # Skip ellipsis statements
            if isinstance(stmt, cst.SimpleStatementLine):
                if any(self._is_ellipsis_expr(s) for s in stmt.body):
                    continue

            effects.append(stmt)

        return effects

    def _create_inline_ellipsis(
        self, trailing_whitespace: cst.TrailingWhitespace | None = None
    ) -> cst.SimpleStatementSuite:
        """Create an inline ellipsis statement with optional trailing whitespace."""
        return cst.SimpleStatementSuite(
            body=[cst.Expr(value=cst.Ellipsis())],
            trailing_whitespace=trailing_whitespace or cst.TrailingWhitespace(),
        )

    def _create_merged_body(
        self,
        effects: list[cst.BaseStatement],
        docstring: cst.SimpleStatementLine | None,
        header: cst.TrailingWhitespace,
        footer: Sequence[cst.EmptyLine],
        trailing_whitespace: cst.TrailingWhitespace | None = None,
    ) -> cst.BaseSuite:
        """Create a function body with merged effects."""
        # If there are no effects and no docstring, use inline ellipsis
        if not effects and not docstring:
            return self._create_inline_ellipsis(trailing_whitespace)

        # Build body statements
        body_stmts: list[cst.BaseStatement] = []
        if docstring:
            body_stmts.append(docstring)

        if effects:
            body_stmts.extend(effects)
        elif docstring:
            # Docstring but no effects - add ellipsis
            body_stmts.append(
                cst.SimpleStatementLine(body=[cst.Expr(value=cst.Ellipsis())])
            )

        # If single effect and no docstring, use inline body
        if len(effects) == 1 and not docstring:
            eff = effects[0]
            if isinstance(eff, cst.SimpleStatementLine):
                # Preserve trailing_whitespace (includes comments) when converting to inline
                return cst.SimpleStatementSuite(
                    body=eff.body,
                    trailing_whitespace=eff.trailing_whitespace,
                )

        return cst.IndentedBlock(body=body_stmts, header=header, footer=footer)

    def _create_ellipsis_body(
        self,
        docstring: cst.SimpleStatementLine | None,
        header: cst.TrailingWhitespace,
        footer: Sequence[cst.EmptyLine],
        trailing_whitespace: cst.TrailingWhitespace | None = None,
    ) -> cst.BaseSuite:
        """Create a function body with just ellipsis (and optionally docstring)."""
        if not docstring:
            # No docstring - use inline ellipsis
            return self._create_inline_ellipsis(trailing_whitespace)

        # Has docstring - use indented block with docstring then ellipsis
        body_stmts = [
            docstring,
            cst.SimpleStatementLine(body=[cst.Expr(value=cst.Ellipsis())]),
        ]

        return cst.IndentedBlock(body=body_stmts, header=header, footer=footer)


def merge_overload_effects(content: str) -> str:
    """
    Merge effects from all overloads into the first overload.
    Replace bodies of subsequent overloads with '...'.
    """
    try:
        module = cst.parse_module(content)
        transformer = OverloadMerger()
        new_module = module.visit(transformer)
        return new_module.code
    except Exception as e:
        print(f"Error parsing with libcst: {e}")
        raise


def main():
    import sys

    if len(sys.argv) < 2:
        print("Usage: merge_overload_effects.py <file.pyi> [<file2.pyi> ...]")
        sys.exit(1)

    for input_file in sys.argv[1:]:
        try:
            with open(input_file, "r") as f:
                content = f.read()

            result = merge_overload_effects(content)

            with open(input_file, "w") as f:
                f.write(result)

            print(f"Successfully merged overload effects in {input_file}")
        except Exception as e:
            print(f"Error processing {input_file}: {e}")


if __name__ == "__main__":
    main()
