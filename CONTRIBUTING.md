# Contributing to Lifeguard for Lazy Imports

Welcome! We’re excited that you’re interested in contributing to Lifeguard. Whether you’re fixing a bug, adding a feature, or improving documentation, your help makes Lifeguard better for everyone.

Development docs are WIP. Please reach out if you are working on an issue and have questions or want a code pointer.

## Cloning the Repository

Lifeguard uses Git submodules, so you need to clone them alongside the main repository. Use the `--recurse-submodules` flag when cloning:

```bash
git clone --recurse-submodules https://github.com/facebook/Lifeguard.git
```

If you've already cloned the repository without the flag, you can initialize the submodules afterwards:

```bash
git submodule update --init --recursive
```

After pulling new changes, submodules may need to be updated:

```bash
git pull
git submodule update --recursive
```

## Getting Started

The [rust toolchain](https://www.rust-lang.org/tools/install) is required for development. You can use the normal `cargo` commands (e.g. `cargo build`, `cargo test`).

## Coding Conventions

We follow the [Buck2 coding conventions](https://github.com/facebook/buck2/blob/main/HACKING.md#coding-conventions), with the caveat that we use our internal error framework for errors reported by the type checker.

## Testing

You can use `cargo test` to run the tests, or `python3 test.py` from this directory.

Tests live in `tests/`, and the harness is `src/test_lib.rs`. Most tests are a
`#[test]` function holding inline Python, where the expected results are written
as `# E:` comments on the line that should produce them:

```rust
#[test]
fn test_module_scope_call() {
    let code = r#"
import os
os.chdir("/home/")  # E: unsafe-function-call
"#;
    check(code);
}
```

The assertion is two-way: an expected error that is not raised fails, and so
does a raised error that was not expected. Put several on one line by repeating
the comment: `# E: imported-var-argument  # E: unsafe-function-call`.

### Which helper to use

| Helper | Use it for |
| --- | --- |
| `check` | Safety errors in a single module named `test`. The common case. |
| `check_all` | Safety errors across several modules, as `(name, code)` pairs. |
| `check_effects` | The effects a single module produces, rather than its verdict. |
| `check_all_effects` | Effects across several modules. |
| `check_effects_as_main` / `check_effects_not_main` / `check_effects_no_main` | Effects under each `__main__` setting, for `if __name__ == "__main__"` handling. |
| `check_errors_and_implicit_imports` | Safety errors plus the implicit imports a module pulls in. |
| `analyze_tree` | The raw per-module `AnalysisMap`, when you need to assert on effects or pending imports directly. |
| `build_import_graph` | Just the import graph, for resolution and cycle questions. |
| `run_lifeguard_analysis` / `run_lifeguard_analysis_with` | The final `LifeGuardAnalysis`, for `lazy_eligible`, `load_imports_eagerly`, and output formatting. Pass `verbose_test_options()` when you need `import_cycles`. |
| `run_analysis_on` | The pipeline over a `TestSources` you built yourself, e.g. one with injected parse errors. |
| `TestSources::with_parse_errors` | Modules that should be treated as unparseable. |

### Writing the Python

Snippets must be valid Python. The harness strips the indentation common to
every line, so you can indent a snippet to match the surrounding Rust, but it
will reject anything that still fails to parse. This matters because the parser
recovers from syntax errors and returns a best-effort AST, so a malformed
snippet would otherwise assert against behaviour that real code never reaches.

Watch for a stray leading space on the first line: it makes the minimum
indentation zero, so nothing is stripped and the rest of the snippet is left
over-indented.

The bundled type stubs are shared across the whole test binary, and a
`TestSources` only admits the stubs its modules can actually reach. Adding an
`import` to a snippet is enough to pull the corresponding stub in.

## Making a Pull Request

Contributing a pull request (PR) is the main way to propose changes to Lifeguard. To ensure your PR is reviewed efficiently and has the best chance of being accepted, please make sure you have done the following:

- [ ] Updated or added new tests to cover your changes (see testing section for details).
- [ ] Made sure all continuous integration (CI) checks pass before requesting a review. Fix any errors or warnings, or ask us about any CI results you don't understand.
- [ ] Written a clear description: Provide a concise summary of what your PR does. Explain the motivation, the approach, and any important details.
- [ ] If your PR addresses a specific issue, reference the issue(s) in the description using the special GitHub keywords (e.g., “Fixes #123”). This will automatically link your PR to the relevant issue and helps us keep track of things.
- [ ] Try to limit your PR to a single purpose or issue. Avoid mixing unrelated changes, as this makes review harder.
- [ ] Clean up any temporary debugging statements or code before submitting.

We aim to respond to all PRs in a timely manner, but please note we prioritise reviews for work that is highest priority (e.g. critical bug fixes, upcoming milestones). If you haven’t received a response to your PR within a week of submitting, you can nudge maintainers by tagging us in a comment or sending a reminder in discord.

## How Pull Requests Are Imported

Lifeguard is developed in Meta's internal repository, which is the source of truth. The GitHub repository is automatically synchronized from the internal repository.

When you open a pull request on GitHub, it is not merged directly into the GitHub repository. Instead, a Meta employee will import the PR into the internal repository using an internal tool. The changes go through internal review and CI, and are then automatically reflected back to GitHub.

Because of this workflow:
- Your PR will not appear as "merged" in the usual GitHub sense, but your changes will appear in the repository once the internal-to-GitHub sync runs.
- The commit that lands internally (and syncs back to GitHub) will be authored by the Meta employee who imported it, not by the original PR author. Your contribution is still attributed in the PR history and in the commit message, but the Git author field will differ from your GitHub account.

### AI Generated Code

We’re excited to see how AI is transforming the way people write code. We encourage contributors to use AI tools to explore, learn, and enhance the Lifeguard codebase. While we generally support the use of AI for creating PRs, please ensure you thoroughly review and understand any AI-generated code before submitting. This practice helps us maintain high code quality standards, facilitates meaningful review discussions with maintainers, and increases the likelihood that your submission will be accepted.

Even if you use AI agents in your workflow, please do not use them to generate PRs directly and ensure you follow our guidelines and code of conduct carefully.

As with manually written code, low-quality or spam PRs written with AI may be rejected. Contributors or agents who repeatedly submit such PRs may be blocked from future contributions.

## Contributor License Agreement ("CLA")

In order to accept your pull request, we need you to submit a CLA. You only need to do this once to work on any of Meta’s open source projects.

Complete your CLA here: <https://code.facebook.com/cla>. If you have any questions, please drop us a line at cla@fb.com.

You are also expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md), so please read that if you are a new contributor.

## License

By contributing to Lifeguard, you agree that your contributions will be licensed under the LICENSE file in the root directory of this source tree.
