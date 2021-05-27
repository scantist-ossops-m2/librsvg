//! SVG paint servers.

use cssparser::Parser;

use crate::bbox::BoundingBox;
use crate::document::{AcquiredNodes, NodeId};
use crate::drawing_ctx::DrawingCtx;
use crate::element::Element;
use crate::error::{
    AcquireError, ImplementationLimit, NodeIdError, ParseError, RenderingError, ValueErrorKind,
};
use crate::gradient::{ResolvedGradient, UserSpaceGradient};
use crate::node::NodeBorrow;
use crate::parsers::Parse;
use crate::pattern::{ResolvedPattern, UserSpacePattern};
use crate::properties::ComputedValues;
use crate::unit_interval::UnitInterval;
use crate::util;

/// Unresolved SVG paint server straight from the DOM data.
///
/// This is either a solid color (which if `currentColor` needs to be extracted from the
/// `ComputedValues`), or a paint server like a gradient or pattern which is referenced by
/// a URL that points to a certain document node.
///
/// Use [`PaintServer.resolve`](#method.resolve) to turn this into a
/// [`PaintSource`](enum.PaintSource.html).
#[derive(Debug, Clone, PartialEq)]
pub enum PaintServer {
    /// For example, `fill="none"`.
    None,

    /// For example, `fill="url(#some_gradient) fallback_color"`.
    Iri {
        iri: Box<NodeId>,
        alternate: Option<cssparser::Color>,
    },

    /// For example, `fill="blue"`.
    SolidColor(cssparser::Color),
}

/// Paint server with resolved references, with unnormalized lengths.
///
/// Use [`PaintSource.to_user_space`](#method.to_user_space) to turn this into a
/// [`UserSpacePaintSource`](enum.UserSpacePaintSource.html).
pub enum PaintSource {
    None,
    Gradient(ResolvedGradient, Option<cssparser::RGBA>),
    Pattern(ResolvedPattern, Option<cssparser::RGBA>),
    SolidColor(cssparser::RGBA),
}

/// Fully resolved paint server, in user-space units.
///
/// This has everything required for rendering.
pub enum UserSpacePaintSource {
    None,
    Gradient(UserSpaceGradient, Option<cssparser::RGBA>),
    Pattern(UserSpacePattern, Option<cssparser::RGBA>),
    SolidColor(cssparser::RGBA),
}

impl Parse for PaintServer {
    fn parse<'i>(parser: &mut Parser<'i, '_>) -> Result<PaintServer, ParseError<'i>> {
        if parser
            .try_parse(|i| i.expect_ident_matching("none"))
            .is_ok()
        {
            Ok(PaintServer::None)
        } else if let Ok(url) = parser.try_parse(|i| i.expect_url()) {
            let loc = parser.current_source_location();

            let alternate = if !parser.is_exhausted() {
                if parser
                    .try_parse(|i| i.expect_ident_matching("none"))
                    .is_ok()
                {
                    None
                } else {
                    Some(parser.try_parse(|i| cssparser::Color::parse(i))?)
                }
            } else {
                None
            };

            Ok(PaintServer::Iri {
                iri: Box::new(
                    NodeId::parse(&url)
                        .map_err(|e: NodeIdError| -> ValueErrorKind { e.into() })
                        .map_err(|e| loc.new_custom_error(e))?,
                ),
                alternate,
            })
        } else {
            Ok(cssparser::Color::parse(parser).map(PaintServer::SolidColor)?)
        }
    }
}

impl PaintServer {
    /// Resolves colors, plus node references for gradients and patterns.
    ///
    /// `opacity` depends on `strokeOpacity` or `fillOpacity` depending on whether
    /// the paint server is for the `stroke` or `fill` properties.
    ///
    /// `current_color` should be the value of `ComputedValues.color()`.
    pub fn resolve(
        &self,
        acquired_nodes: &mut AcquiredNodes<'_>,
        opacity: UnitInterval,
        current_color: cssparser::RGBA,
    ) -> Result<PaintSource, RenderingError> {
        match self {
            PaintServer::Iri {
                ref iri,
                ref alternate,
            } => acquired_nodes
                .acquire(iri)
                .and_then(|acquired| {
                    let node = acquired.get();
                    assert!(node.is_element());

                    match *node.borrow_element() {
                        Element::LinearGradient(ref g) => {
                            g.resolve(&node, acquired_nodes, opacity).map(|g| {
                                PaintSource::Gradient(
                                    g,
                                    alternate.map(|c| resolve_color(&c, opacity, current_color)),
                                )
                            })
                        }
                        Element::Pattern(ref p) => {
                            p.resolve(&node, acquired_nodes, opacity).map(|p| {
                                PaintSource::Pattern(
                                    p,
                                    alternate.map(|c| resolve_color(&c, opacity, current_color)),
                                )
                            })
                        }
                        Element::RadialGradient(ref g) => {
                            g.resolve(&node, acquired_nodes, opacity).map(|g| {
                                PaintSource::Gradient(
                                    g,
                                    alternate.map(|c| resolve_color(&c, opacity, current_color)),
                                )
                            })
                        }
                        _ => Err(AcquireError::InvalidLinkType(iri.as_ref().clone())),
                    }
                })
                .or_else(|err| match (err, alternate) {
                    (AcquireError::MaxReferencesExceeded, _) => {
                        rsvg_log!("exceeded maximum number of referenced objects");
                        Err(RenderingError::LimitExceeded(
                            ImplementationLimit::TooManyReferencedElements,
                        ))
                    }

                    // The following two cases catch AcquireError::CircularReference, which for
                    // paint servers may mean that there is a pattern or gradient with a reference
                    // cycle in its "href" attribute.  This is an invalid paint server, and per
                    // https://www.w3.org/TR/SVG2/painting.html#SpecifyingPaint we should try to
                    // fall back to the alternate color.
                    (_, Some(color)) => {
                        rsvg_log!(
                            "could not resolve paint server \"{}\", using alternate color",
                            iri
                        );

                        Ok(PaintSource::SolidColor(resolve_color(
                            color,
                            opacity,
                            current_color,
                        )))
                    }

                    (_, _) => {
                        rsvg_log!(
                            "could not resolve paint server \"{}\", no alternate color specified",
                            iri
                        );

                        Ok(PaintSource::None)
                    }
                }),

            PaintServer::SolidColor(color) => Ok(PaintSource::SolidColor(resolve_color(
                color,
                opacity,
                current_color,
            ))),

            PaintServer::None => Ok(PaintSource::None),
        }
    }
}

impl PaintSource {
    /// Converts lengths to user-space.
    pub fn to_user_space(
        &self,
        bbox: &BoundingBox,
        draw_ctx: &DrawingCtx,
        values: &ComputedValues,
    ) -> UserSpacePaintSource {
        match *self {
            PaintSource::None => UserSpacePaintSource::None,
            PaintSource::SolidColor(c) => UserSpacePaintSource::SolidColor(c),

            PaintSource::Gradient(ref g, c) => match (g.to_user_space(bbox, draw_ctx, values), c) {
                (Some(gradient), c) => UserSpacePaintSource::Gradient(gradient, c),
                (None, Some(c)) => UserSpacePaintSource::SolidColor(c),
                (None, None) => UserSpacePaintSource::None,
            },

            PaintSource::Pattern(ref p, c) => match (p.to_user_space(bbox, draw_ctx, values), c) {
                (Some(pattern), c) => UserSpacePaintSource::Pattern(pattern, c),
                (None, Some(c)) => UserSpacePaintSource::SolidColor(c),
                (None, None) => UserSpacePaintSource::None,
            },
        }
    }
}

/// Resolves a CSS color into an RGBA value.
///
/// A CSS color can be `currentColor`, in which case the computed value comes from
/// the `color` property.  You should pass the `color` property's value for `current_color`.
pub fn resolve_color(
    color: &cssparser::Color,
    opacity: UnitInterval,
    current_color: cssparser::RGBA,
) -> cssparser::RGBA {
    let rgba = match *color {
        cssparser::Color::RGBA(rgba) => rgba,
        cssparser::Color::CurrentColor => current_color,
    };

    let UnitInterval(o) = opacity;

    let alpha = (f64::from(rgba.alpha) * o).round();
    let alpha = util::clamp(alpha, 0.0, 255.0);

    // For the following I'd prefer to use `cast::u8(alpha).unwrap()`
    // but the cast crate is erroneously returning Overflow for `u8(255.0)`:
    // https://github.com/japaric/cast.rs/issues/23

    let alpha = alpha as u8;

    cssparser::RGBA { alpha, ..rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_invalid_syntax() {
        assert!(PaintServer::parse_str("").is_err());
        assert!(PaintServer::parse_str("42").is_err());
        assert!(PaintServer::parse_str("invalid").is_err());
    }

    #[test]
    fn parses_none() {
        assert_eq!(PaintServer::parse_str("none").unwrap(), PaintServer::None);
    }

    #[test]
    fn parses_solid_color() {
        assert_eq!(
            PaintServer::parse_str("rgb(255, 128, 64, 0.5)").unwrap(),
            PaintServer::SolidColor(cssparser::Color::RGBA(cssparser::RGBA::new(
                255, 128, 64, 128
            )))
        );

        assert_eq!(
            PaintServer::parse_str("currentColor").unwrap(),
            PaintServer::SolidColor(cssparser::Color::CurrentColor)
        );
    }

    #[test]
    fn parses_iri() {
        assert_eq!(
            PaintServer::parse_str("url(#link)").unwrap(),
            PaintServer::Iri {
                iri: Box::new(NodeId::Internal("link".to_string())),
                alternate: None,
            }
        );

        assert_eq!(
            PaintServer::parse_str("url(foo#link) none").unwrap(),
            PaintServer::Iri {
                iri: Box::new(NodeId::External("foo".to_string(), "link".to_string())),
                alternate: None,
            }
        );

        assert_eq!(
            PaintServer::parse_str("url(#link) #ff8040").unwrap(),
            PaintServer::Iri {
                iri: Box::new(NodeId::Internal("link".to_string())),
                alternate: Some(cssparser::Color::RGBA(cssparser::RGBA::new(
                    255, 128, 64, 255
                ))),
            }
        );

        assert_eq!(
            PaintServer::parse_str("url(#link) rgb(255, 128, 64, 0.5)").unwrap(),
            PaintServer::Iri {
                iri: Box::new(NodeId::Internal("link".to_string())),
                alternate: Some(cssparser::Color::RGBA(cssparser::RGBA::new(
                    255, 128, 64, 128
                ))),
            }
        );

        assert_eq!(
            PaintServer::parse_str("url(#link) currentColor").unwrap(),
            PaintServer::Iri {
                iri: Box::new(NodeId::Internal("link".to_string())),
                alternate: Some(cssparser::Color::CurrentColor),
            }
        );

        assert!(PaintServer::parse_str("url(#link) invalid").is_err());
    }

    #[test]
    fn resolves_explicit_color() {
        use cssparser::{Color, RGBA};

        assert_eq!(
            resolve_color(
                &Color::RGBA(RGBA::new(255, 0, 0, 128)),
                UnitInterval::clamp(0.5),
                RGBA::new(0, 255, 0, 255)
            ),
            RGBA::new(255, 0, 0, 64),
        );
    }

    #[test]
    fn resolves_current_color() {
        use cssparser::{Color, RGBA};

        assert_eq!(
            resolve_color(
                &Color::CurrentColor,
                UnitInterval::clamp(0.5),
                RGBA::new(0, 255, 0, 128)
            ),
            RGBA::new(0, 255, 0, 64),
        );
    }
}
