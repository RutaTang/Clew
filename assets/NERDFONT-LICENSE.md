# Symbols Nerd Font Mono — license and attributions

`assets/SymbolsNerdFontMono-Regular.ttf` is the symbols-only font of the
[Nerd Fonts](https://github.com/ryanoasis/nerd-fonts) project, release 3.4.0
(the font's own name table reads "Symbols Nerd Font Mono 3.4.0" and
"Copyright (c) 2016, Ryan McIntyre"). clew embeds it, unmodified, to draw the
file-type icons in the file tree. It is shipped inside Clew.app together with
this notice and the license texts it refers to (`Contents/Resources/Licenses/`,
see `scripts/build-app.sh`).

## The font

Nerd Fonts licenses its patched and glyph fonts under the SIL Open Font
License, Version 1.1:

> Copyright (c) 2014, Ryan L McIntyre (https://ryanlmcintyre.com).
>
> This Font Software is licensed under the SIL Open Font License, Version 1.1.

The full license text is in [`licenses/OFL-1.1.txt`](licenses/OFL-1.1.txt)
(also at https://openfontlicense.org). Under it the font may be used, bundled
and redistributed with software, but not sold by itself, and a modified
version must not use the Reserved Font Names.

Nerd Fonts' own source files (not included here) are MIT-licensed,
Copyright (c) 2014 Ryan L McIntyre; the license text is reproduced below,
under "MIT notices".

## The glyph sets inside it

The font aggregates icon sets from other projects, each under its own
license. As listed in the Nerd Fonts 3.4.0 license audit
(https://github.com/ryanoasis/nerd-fonts/blob/v3.4.0/license-audit.md):

| Glyph set                               | License                                          |
| --------------------------------------- | ------------------------------------------------ |
| Codicons (Microsoft)                    | CC BY 4.0                                        |
| Devicons                                | MIT                                              |
| Font Awesome (the icons)                | CC BY 4.0                                        |
| Font Awesome Extension                  | MIT                                              |
| Font Logos                              | The Unlicense                                    |
| IEC Power Symbols                       | MIT                                              |
| Material Design Icons                   | Apache License 2.0                               |
| Seti-UI (modified), Nerd Fonts original | MIT                                              |
| Octicons (GitHub)                       | MIT                                              |
| Pomicons                                | SIL OFL 1.1                                      |
| Powerline Extra Symbols                 | MIT                                              |
| Powerline Symbols                       | MIT (the audit says "free license")              |
| Weather Icons                           | SIL OFL 1.1                                      |

- **CC BY 4.0** (Codicons, Font Awesome icons): the icons are used unmodified
  and attributed here to their authors, under
  https://creativecommons.org/licenses/by/4.0/.
- **Apache License 2.0** (Material Design Icons): the full text is in
  [`licenses/Apache-2.0.txt`](licenses/Apache-2.0.txt). The icons are used
  unmodified, as part of the font.
- **SIL OFL 1.1** (Pomicons, Weather Icons): see
  [`licenses/OFL-1.1.txt`](licenses/OFL-1.1.txt).
- **MIT**: each project's copyright notice and the license's permission
  notice are reproduced under "MIT notices" below, as the license requires.
- **The Unlicense** (Font Logos, https://github.com/Lukas-W/font-logos): a
  public-domain dedication; no notice is required.

## MIT notices

The copyright notices of the MIT-licensed works in the font, as each project
states it (checked against the upstream license files):

- Nerd Fonts (the font patcher, the original icons):
  Copyright (c) 2014 Ryan L McIntyre — https://github.com/ryanoasis/nerd-fonts
- Seti-UI: Copyright (c) 2014 Jesse Weed — https://github.com/jesseweed/seti-ui
- Devicons, the current set: Copyright (c) 2015 konpa —
  https://github.com/devicons/devicon
- Devicons, the original set: by Theodore Vorillas, "licensed under MIT" (the
  project states no copyright line) — https://github.com/vorillaz/devicons
- Font Awesome Extension: Copyright (c) 2017 André Luiz Gava —
  https://github.com/AndreLZGava/font-awesome-extension
- IEC Power Symbols: Copyright (c) 2013 Joe Loughry —
  https://github.com/jloughry/Unicode
- Octicons: Copyright (c) 2023 GitHub Inc. — https://github.com/primer/octicons
- Powerline Extra Symbols: Copyright (c) 2016 Ryan L McIntyre —
  https://github.com/ryanoasis/powerline-extra-symbols
- Powerline Symbols: Copyright 2013 Kim Silkebækken and other contributors —
  https://github.com/powerline/powerline

Each of these works is licensed under the MIT License:

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

## Rust crates

The Rust crates compiled into clew and clew-server carry their own licenses;
their notices are collected at build time into `THIRD-PARTY-NOTICES.txt` in the
same `Licenses/` folder of the app bundle (`scripts/third-party-notices.sh`).
