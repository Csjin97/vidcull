
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));


export const DESIGN_MD_PATH = resolve(here, "..", "..", "docs/design-system.md");

export const TOKENS_CSS_PATH = resolve(here, "tokens.css");

export interface DesignTokens {
  colors: Record<string, string>;
  rounded: Record<string, string>;
  spacing: Record<string, string>;
  typography: Record<string, Record<string, string>>;
}

function stripQuotes(value: string): string {
  const t = value.trim();
  if (
    (t.startsWith('"') && t.endsWith('"')) ||
    (t.startsWith("'") && t.endsWith("'"))
  ) {
    return t.slice(1, -1);
  }
  return t;
}


export function parseDesignTokens(markdown: string): DesignTokens {
  markdown = markdown.replace(/\r\n/g, "\n");
  const frontmatter = markdown.match(/^---\n([\s\S]*?)\n---/);
  if (!frontmatter) {
    throw new Error("docs/design-system.md frontmatter (--- … ---) not found");
  }

  const tokens: DesignTokens = {
    colors: {},
    rounded: {},
    spacing: {},
    typography: {},
  };

  let section: string | null = null;
  let typographyToken: string | null = null;

  for (const rawLine of frontmatter[1].split("\n")) {
    if (rawLine.trim() === "") {
      continue;
    }
    const indent = rawLine.length - rawLine.trimStart().length;
    const content = rawLine.trim();
    const colon = content.indexOf(":");
    if (colon === -1) {
      continue;
    }
    const key = content.slice(0, colon).trim();
    const value = content.slice(colon + 1).trim();

    if (indent === 0) {
      section = key;
      typographyToken = null;
      continue;
    }

    if (section === "colors" || section === "rounded" || section === "spacing") {
      if (indent === 2 && value !== "") {
        tokens[section][key] = stripQuotes(value);
      }
      continue;
    }

    if (section === "typography") {
      if (indent === 2 && value === "") {
        typographyToken = key;
        tokens.typography[typographyToken] = {};
      } else if (indent === 4 && typographyToken !== null && value !== "") {
        tokens.typography[typographyToken][key] = stripQuotes(value);
      }
    }
  }

  return tokens;
}


export function expectedCssVariables(tokens: DesignTokens): Map<string, string> {
  const variables = new Map<string, string>();

  for (const [name, value] of Object.entries(tokens.colors)) {
    variables.set(`--color-${name}`, value);
  }
  for (const [name, value] of Object.entries(tokens.spacing)) {
    variables.set(`--space-${name}`, value);
  }
  for (const [name, value] of Object.entries(tokens.rounded)) {
    variables.set(`--radius-${name}`, value);
  }
  for (const [token, props] of Object.entries(tokens.typography)) {
    if (props.fontSize !== undefined) {
      variables.set(`--type-${token}-size`, props.fontSize);
    }
    if (props.fontWeight !== undefined) {
      variables.set(`--type-${token}-weight`, props.fontWeight);
    }
    if (props.lineHeight !== undefined) {
      variables.set(`--type-${token}-line`, props.lineHeight);
    }
    if (props.letterSpacing !== undefined) {
      variables.set(`--type-${token}-tracking`, props.letterSpacing);
    }
    if (props.textTransform !== undefined) {
      variables.set(`--type-${token}-transform`, props.textTransform);
    }
  }

  return variables;
}


export function parseCssVariables(css: string): Map<string, string> {
  const root = css.match(/:root\s*\{([\s\S]*?)\}/);
  if (!root) {
    throw new Error(":root { … } block not found in stylesheet");
  }
  const variables = new Map<string, string>();
  const declaration = /(--[a-z0-9-]+)\s*:\s*([^;]+);/gi;
  let match: RegExpExecArray | null;
  while ((match = declaration.exec(root[1])) !== null) {
    variables.set(match[1].trim(), match[2].trim());
  }
  return variables;
}


export function loadDesignTokens(): DesignTokens {
  return parseDesignTokens(readFileSync(DESIGN_MD_PATH, "utf8"));
}


export function loadCssVariables(): Map<string, string> {
  return parseCssVariables(readFileSync(TOKENS_CSS_PATH, "utf8"));
}
