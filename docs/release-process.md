# E-Cash App Release Process

## Release Candidates
- Create release branch (if new major/minor): `git checkout -b releases/vX.Y`
- Update versions:
  - `pubspec.yaml`: `version: X.Y.Z-rc.N`
  - `rust/ecashapp/Cargo.toml`: `version = "X.Y.Z-rc.N"`
  - `just build-linux`
  - `flutter analyze`
- Commit: `git commit -am "chore: bump version to vX.Y.Z-rc.N"`
- Push: `git push upstream releases/vX.Y`
- Tag: `git tag -a -s vX.Y.Z-rc.N`
- Push tag: `git push upstream vX.Y.Z-rc.N`
- Verify GitHub release created with APK

## Final Release
- On release branch: `git checkout releases/vX.Y`
- Update versions:
  - `pubspec.yaml`: `version: X.Y.Z`
  - `rust/ecashapp/Cargo.toml`: `version = "X.Y.Z"`
- Commit: `git commit -am "chore: bump version to vX.Y.Z"`
- Push: `git push upstream releases/vX.Y`
- Tag: `git tag -a -s vX.Y.Z`
- Push tag: `git push upstream vX.Y.Z`
- Verify GitHub release created with APK

## Post-Release
- Add branch protection to `releases/vX.Y` (first release only)
- PR to bump master to next alpha:
  - `pubspec.yaml`: `version: X.(Y+1).0-alpha`
  - `rust/ecashapp/Cargo.toml`: `version = "X.(Y+1).0-alpha"`

## Version Code Reference
- RC: `major*1000000 + minor*10000 + patch*100 + rc_num`
- Final: `major*1000000 + minor*10000 + patch*100 + 90`
