# Contributing

Bug fixes, documentation fixes, and focused improvements are welcome. Aria
is a maintainer-led project. Changes are reviewed for correctness, how they
fit Aria's direction, and what they'll take to maintain. Reviews happen as
time allows, and not every proposal is accepted.

## Before you start

Small fixes can go straight to a pull request. For a new feature or a larger
change, open an issue first and agree on the scope.

## Making changes

- Keep each pull request to one change.
- Run `just check` before you push. If you changed `clients/`, also run
  `just swift-check` on macOS.
- The checks need [just](https://just.systems), [Taplo](https://taplo.tamasfe.dev),
  [cargo-shear](https://github.com/Boshen/cargo-shear), and
  [uv](https://docs.astral.sh/uv/). The app checks need Xcode and
  [XcodeGen](https://github.com/yonaskolb/XcodeGen). On Linux, install the
  system packages that [the CI workflow](.github/workflows/check.yml) lists.
- In the description, explain the problem, the change, and how you checked
  it.
- Use `type(scope): description` for the PR title, for example
  `fix(setup): skip missing wake models`. Pull requests are squash-merged,
  so the title becomes the commit message.
- AI-assisted changes are welcome if you understand them and can explain
  them.

[`.rules`](.rules) lists the commands and conventions in more detail.

## License and CLA

Your contribution uses the same license as the files you change. The
[License](README.md#license) section of the README lists which parts use
which license.

Every contributor signs the
[Aria CLA](https://cla-assistant.io/Farhad-Shabani/aria) once. A bot will ask
you to sign it on your first pull request. You keep the copyright in your
work. The CLA grants the maintainer a license to use it, which also lets
your contribution ship in builds under other licenses, such as the official
Mac app.
