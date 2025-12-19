# Conda/Bioconda Recipe

This directory contains a template recipe for submitting rebgzf to bioconda.

## Submission Steps

1. **Publish to crates.io** (if not already done):
   ```bash
   cargo publish
   ```

2. **Create a GitHub release**:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

3. **Get the tarball SHA256**:
   ```bash
   curl -sL https://github.com/nh13/rebgzf/archive/refs/tags/v0.1.0.tar.gz | sha256sum
   ```

4. **Fork bioconda-recipes**:
   ```bash
   gh repo fork bioconda/bioconda-recipes --clone
   cd bioconda-recipes
   ```

5. **Create recipe**:
   ```bash
   mkdir -p recipes/rebgzf
   cp /path/to/rebgzf/conda/meta.yaml recipes/rebgzf/
   # Update SHA256 in meta.yaml
   ```

6. **Submit PR**:
   ```bash
   git checkout -b add-rebgzf
   git add recipes/rebgzf/
   git commit -m "Add rebgzf: gzip to BGZF transcoder"
   git push origin add-rebgzf
   gh pr create --repo bioconda/bioconda-recipes
   ```

## Testing Locally

With bioconda-utils installed:
```bash
bioconda-utils build recipes config.yml --packages rebgzf
```

## Notes

- The recipe uses `cargo-bundle-licenses` to handle Rust dependency licenses
- Pre-built binaries from cargo-dist can speed up builds (see `.github/workflows/release.yml`)
- After bioconda acceptance, the package will be available via:
  ```bash
  conda install -c bioconda rebgzf
  ```
