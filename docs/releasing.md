# Releasing HomeCmdr

## Day-to-day development

Commit freely to `main`. No special branch needed.

```bash
git add .
git commit -m "feat: your change"
git push
```

## Cutting a release

### 1. Bump the version (if needed)

Update `version` in every crate `Cargo.toml` that changed. For a coordinated workspace bump, update them all together.

```toml
# crates/*/Cargo.toml
version = "0.2.0"
```

Commit the bump:

```bash
git add .
git commit -m "chore: bump version to 0.2.0"
git push
```

### 2. Tag and push

```bash
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
```

### 3. Create the GitHub Release

Auto-generate release notes from commits since the last tag:

```bash
gh release create v0.2.0 --generate-notes
```

Or write your own notes:

```bash
gh release create v0.2.0 --title "v0.2.0" --notes "Summary of what changed."
```

---

## Versioning rules

- `main` = development / latest
- Tags = releases (`v0.1.0`, `v0.2.0`, `v1.0.0`, ...)
- Follow [SemVer](https://semver.org/): `MAJOR.MINOR.PATCH`
  - `PATCH` — bug fixes, no API changes
  - `MINOR` — new functionality, backwards compatible
  - `MAJOR` — breaking changes

## homecmdr-dash

The dashboard repo (`github.com/homecmdr/homecmdr-dash`) is versioned independently using the same steps above. There is no requirement to keep versions in sync with `homecmdr-api`.
