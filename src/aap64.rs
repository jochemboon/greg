//! The built-in "AAP-64" colour scheme, made only from colours of Adigun A.
//! Polack's 64-colour AAP-64 palette: syntax colours plus the colours of the
//! editor and the surrounding UI.

use crate::highlight::{ThemeColors, UiPalette};
use eframe::egui::Color32;
use std::str::FromStr;
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};

pub const NAME: &str = "AAP-64";

/// The palette, in its published order (lospec.com/palette-list/aap-64).
pub const PALETTE: [u32; 64] = [
    0x060608, 0x141013, 0x3b1725, 0x73172d, 0xb4202a, 0xdf3e23, 0xfa6a0a, 0xf9a31b, //
    0xffd541, 0xfffc40, 0xd6f264, 0x9cdb43, 0x59c135, 0x14a02e, 0x1a7a3e, 0x24523b, //
    0x122020, 0x143464, 0x285cc4, 0x249fde, 0x20d6c7, 0xa6fcdb, 0xffffff, 0xfef3c0, //
    0xfad6b8, 0xf5a097, 0xe86a73, 0xbc4a9b, 0x793a80, 0x403353, 0x242234, 0x221c1a, //
    0x322b28, 0x71413b, 0xbb7547, 0xdba463, 0xf4d29c, 0xdae0ea, 0xb3b9d1, 0x8b93af, //
    0x6d758d, 0x4a5462, 0x333941, 0x422433, 0x5b3138, 0x8e5252, 0xba756a, 0xe9b5a3, //
    0xe3e6ff, 0xb9bffb, 0x849be4, 0x588dbe, 0x477d85, 0x23674e, 0x328464, 0x5daf8d, //
    0x92dcba, 0xcdf7e2, 0xe4d2aa, 0xc7b08b, 0xa08662, 0x796755, 0x5a4e44, 0x423934, //
];

// The colours this scheme uses, by role.
const BLACK: u32 = 0x060608;
const NEAR_BLACK: u32 = 0x141013;
const NIGHT: u32 = 0x242234; // editor background
const DUSK: u32 = 0x403353; // selection, borders
const PLUM: u32 = 0x793a80;
const SLATE: u32 = 0x333941;
const MIST: u32 = 0xdae0ea; // text
const HAZE: u32 = 0xb3b9d1;
const STEEL: u32 = 0x8b93af; // comments
const PEWTER: u32 = 0x6d758d; // line numbers
const SNOW: u32 = 0xe3e6ff;
const WHITE: u32 = 0xffffff;
const RED: u32 = 0xdf3e23;
const CRIMSON: u32 = 0xb4202a;
const ORANGE: u32 = 0xfa6a0a;
const AMBER: u32 = 0xf9a31b;
const YELLOW: u32 = 0xffd541;
const GREEN: u32 = 0x9cdb43;
const GRASS: u32 = 0x59c135;
const SKY: u32 = 0x249fde;
const PERIWINKLE: u32 = 0x849be4;
const LAVENDER: u32 = 0xb9bffb;
const TEAL: u32 = 0x20d6c7;
const MINT: u32 = 0xa6fcdb;
const SEAFOAM: u32 = 0x5daf8d;
const SALMON: u32 = 0xe86a73;
const PEACH: u32 = 0xf5a097;
const CREAM: u32 = 0xf4d29c;
const SAND: u32 = 0xdba463;

fn rgb(hex: u32) -> Color {
    debug_assert!(PALETTE.contains(&hex), "{hex:06x} is not an AAP-64 colour");
    Color {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 0xff,
    }
}

fn c32(hex: u32) -> Color32 {
    let c = rgb(hex);
    Color32::from_rgb(c.r, c.g, c.b)
}

fn translucent(hex: u32, alpha: u8) -> Color32 {
    let c = rgb(hex);
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, alpha)
}

/// Syntax rules: (scope selectors, foreground, font style).
const RULES: &[(&str, u32, FontStyle)] = &[
    (
        "comment, punctuation.definition.comment",
        STEEL,
        FontStyle::ITALIC,
    ),
    (
        "string, punctuation.definition.string",
        GREEN,
        FontStyle::empty(),
    ),
    (
        "constant.character.escape, string.regexp, constant.other.placeholder",
        MINT,
        FontStyle::empty(),
    ),
    ("constant.numeric", ORANGE, FontStyle::empty()),
    (
        "constant.language, constant.other, support.constant",
        PEACH,
        FontStyle::empty(),
    ),
    (
        "keyword, storage, keyword.operator.word, keyword.operator.new",
        SALMON,
        FontStyle::empty(),
    ),
    (
        "keyword.operator, punctuation.separator",
        HAZE,
        FontStyle::empty(),
    ),
    (
        "entity.name.function, support.function, meta.function-call.generic, variable.function",
        SKY,
        FontStyle::empty(),
    ),
    (
        "entity.name.function.macro, support.macro, entity.name.macro, meta.macro",
        TEAL,
        FontStyle::empty(),
    ),
    (
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, \
         entity.name.trait, entity.name.interface, entity.name.union, support.type, \
         support.class, entity.other.inherited-class, storage.type.primitive, \
         storage.type.numeric, storage.type.boolean, storage.type.string",
        YELLOW,
        FontStyle::empty(),
    ),
    (
        "entity.name.namespace, entity.name.module, entity.name.package, \
         meta.path entity.name, support.module",
        SAND,
        FontStyle::empty(),
    ),
    ("variable.parameter", CREAM, FontStyle::empty()),
    (
        "variable.language, variable.parameter.self",
        LAVENDER,
        FontStyle::ITALIC,
    ),
    (
        "meta.attribute, meta.annotation, entity.name.function.decorator, \
         punctuation.definition.annotation, punctuation.decorator",
        SEAFOAM,
        FontStyle::empty(),
    ),
    (
        "support.type.property-name, meta.object-literal.key, entity.name.tag.yaml, \
         variable.other.property, variable.other.member, meta.mapping.key string",
        PERIWINKLE,
        FontStyle::empty(),
    ),
    ("entity.name.tag", SALMON, FontStyle::empty()),
    ("entity.other.attribute-name", AMBER, FontStyle::empty()),
    (
        "entity.name.label, punctuation.definition.label",
        AMBER,
        FontStyle::ITALIC,
    ),
    (
        "markup.heading, entity.name.section",
        YELLOW,
        FontStyle::BOLD,
    ),
    ("markup.bold", MIST, FontStyle::BOLD),
    ("markup.italic", MIST, FontStyle::ITALIC),
    (
        "markup.underline.link, string.other.link",
        SKY,
        FontStyle::UNDERLINE,
    ),
    ("markup.raw, markup.inline.raw", TEAL, FontStyle::empty()),
    ("markup.quote", STEEL, FontStyle::ITALIC),
    ("markup.inserted", GRASS, FontStyle::empty()),
    ("markup.deleted", RED, FontStyle::empty()),
    ("markup.changed", AMBER, FontStyle::empty()),
];

pub fn theme() -> Theme {
    let mut scopes: Vec<ThemeItem> = RULES
        .iter()
        .map(|&(sel, fg, style)| ThemeItem {
            scope: ScopeSelectors::from_str(sel).expect("valid scope selector"),
            style: StyleModifier {
                foreground: Some(rgb(fg)),
                background: None,
                font_style: Some(style),
            },
        })
        .collect();
    scopes.push(ThemeItem {
        scope: ScopeSelectors::from_str("invalid").expect("valid scope selector"),
        style: StyleModifier {
            foreground: Some(rgb(WHITE)),
            background: Some(rgb(CRIMSON)),
            font_style: None,
        },
    });
    Theme {
        name: Some(NAME.into()),
        author: Some("Slate, using the AAP-64 palette by Adigun A. Polack".into()),
        settings: ThemeSettings {
            foreground: Some(rgb(MIST)),
            background: Some(rgb(NIGHT)),
            caret: Some(rgb(YELLOW)),
            selection: Some(rgb(DUSK)),
            gutter_foreground: Some(rgb(PEWTER)),
            accent: Some(rgb(SKY)),
            ..Default::default()
        },
        scopes,
    }
}

/// Editor and UI colours beyond what a syntect theme describes.
pub fn adjust(c: &mut ThemeColors) {
    c.line_highlight = translucent(DUSK, 110);
    c.find_match = translucent(AMBER, 80);
    c.error = c32(RED);
    c.warning = c32(AMBER);
    c.info = c32(SKY);
    c.hint = c32(STEEL);
    c.ui = Some(UiPalette {
        panel: c32(NEAR_BLACK),
        deep: c32(BLACK),
        border: c32(DUSK),
        widget: c32(NIGHT),
        widget_hover: c32(DUSK),
        widget_active: c32(PLUM),
        faint: c32(SLATE),
        text: c32(HAZE),
        strong_text: c32(SNOW),
        selection: c32(DUSK),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_has_64_distinct_colours() {
        let mut p = PALETTE.to_vec();
        p.sort();
        p.dedup();
        assert_eq!(p.len(), 64);
    }

    #[test]
    fn theme_builds_from_palette_colours() {
        // `rgb` asserts (in debug builds) that every colour is in the palette.
        let t = theme();
        assert_eq!(t.scopes.len(), RULES.len() + 1);
        let mut c = crate::highlight::Highlighter::new(NAME).colors();
        adjust(&mut c);
        assert_eq!(c.background, c32(NIGHT));
    }
}
