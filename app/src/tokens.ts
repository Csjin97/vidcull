

export const COLOR_TOKENS = [
  "primary",
  "primary-active",
  "primary-hover",
  "ink",
  "body",
  "body-strong",
  "body-on-light",
  "muted",
  "muted-soft",
  "hairline",
  "hairline-on-light",
  "hairline-soft",
  "canvas",
  "canvas-elevated",
  "canvas-light",
  "surface-card",
  "surface-soft-light",
  "surface-strong-light",
  "on-primary",
  "on-dark",
  "on-light",
  "accent-yellow-hypersail",
  "accent-yellow",
  "semantic-info",
  "semantic-success",
  "semantic-warning",
] as const;


export const SPACING_TOKENS = [
  "xxxs",
  "xxs",
  "xs",
  "sm",
  "md",
  "lg",
  "xl",
  "xxl",
  "super",
] as const;


export const RADIUS_TOKENS = [
  "none",
  "xs",
  "sm",
  "md",
  "lg",
  "xl",
  "full",
] as const;


export const TYPOGRAPHY_TOKENS = [
  "display-mega",
  "display-xl",
  "display-lg",
  "display-md",
  "title-md",
  "title-sm",
  "body-md",
  "body-sm",
  "caption",
  "caption-uppercase",
  "button",
  "nav-link",
  "number-display",
] as const;

export type ColorToken = (typeof COLOR_TOKENS)[number];
export type SpacingToken = (typeof SPACING_TOKENS)[number];
export type RadiusToken = (typeof RADIUS_TOKENS)[number];
export type TypographyToken = (typeof TYPOGRAPHY_TOKENS)[number];

export const colorVar = (token: ColorToken): string => `--color-${token}`;
export const spaceVar = (token: SpacingToken): string => `--space-${token}`;
export const radiusVar = (token: RadiusToken): string => `--radius-${token}`;


export type TypeFacet = "size" | "weight" | "line" | "tracking" | "transform";
export const typeVar = (token: TypographyToken, facet: TypeFacet): string =>
  `--type-${token}-${facet}`;


export function readToken(name: string): string {
  if (typeof document === "undefined") {
    return "";
  }
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}
