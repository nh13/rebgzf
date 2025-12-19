# Contributing to gz_to_bgzf

Thank you for your interest in contributing! This document provides guidelines for contributing to the project.

## Bug Reports

When filing a bug report, please include:

1. **Version information**: Output of `gz2bgzf --version`
2. **Operating system**: e.g., Ubuntu 22.04, macOS 14
3. **Steps to reproduce**: Minimal commands/code to reproduce the issue
4. **Expected behavior**: What you expected to happen
5. **Actual behavior**: What actually happened
6. **Sample data**: If possible, provide a minimal test file that reproduces the issue

## Feature Requests

For feature requests, please describe:

1. **Use case**: What problem are you trying to solve?
2. **Proposed solution**: How do you envision the feature working?
3. **Alternatives**: Have you considered other approaches?

## Pull Requests

### Before Submitting

1. **Check for existing issues/PRs**: Search to avoid duplicates
2. **Open an issue first**: For significant changes, discuss the approach before implementing
3. **Keep PRs focused**: One feature or fix per PR

### Development Workflow

1. Fork the repository
2. Create a feature branch from `main`:
   ```bash
   git checkout -b feature/my-feature
   ```
3. Make your changes
4. Run the test suite:
   ```bash
   cargo test
   ```
5. Run linting and formatting:
   ```bash
   cargo fmt
   cargo clippy --all-targets --all-features -- -D warnings
   ```
6. Commit with a descriptive message
7. Push to your fork and open a PR

### Commit Messages

Use conventional commit format:
- `feat: add new feature`
- `fix: resolve bug`
- `docs: update documentation`
- `test: add tests`
- `refactor: restructure code`
- `perf: improve performance`

### Code Style

- Follow Rust idioms and best practices
- Run `cargo fmt` before committing
- Ensure `cargo clippy` passes with no warnings
- Add tests for new functionality
- Document public APIs with doc comments

### Testing

- All new features should include tests
- Bug fixes should include regression tests
- Run the full test suite before submitting:
  ```bash
  cargo test --all-features
  ```

## Code of Conduct

Be respectful and constructive in all interactions. We're all here to build great software together.

## Questions?

If you have questions about contributing, feel free to open a discussion or issue.
