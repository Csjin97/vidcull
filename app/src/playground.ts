
import "./playground.css";
import {
  COLOR_TOKENS,
  RADIUS_TOKENS,
  SPACING_TOKENS,
  TYPOGRAPHY_TOKENS,
  colorVar,
  radiusVar,
  readToken,
  spaceVar,
  typeVar,
  type TypographyToken,
} from "./tokens";

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className !== undefined) {
    node.className = className;
  }
  if (text !== undefined) {
    node.textContent = text;
  }
  return node;
}

function section(title: string): HTMLElement {
  const wrap = el("section", "pg-section");
  wrap.append(el("h2", undefined, title));
  return wrap;
}

function renderColors(): HTMLElement {
  const wrap = section("Colours");
  const grid = el("div", "pg-swatches");
  for (const token of COLOR_TOKENS) {
    const value = readToken(colorVar(token));
    const swatch = el("div", "pg-swatch");
    const chip = el("div", "pg-swatch__chip");
    chip.style.background = value;
    const meta = el("div", "pg-swatch__meta");
    meta.append(
      el("div", "pg-swatch__name", token),
      el("div", "pg-swatch__value", value),
    );
    swatch.append(chip, meta);
    grid.append(swatch);
  }
  wrap.append(grid);
  return wrap;
}

function applyTypeFacets(node: HTMLElement, token: TypographyToken): void {
  node.style.fontFamily = readToken("--font-sans");
  node.style.fontSize = readToken(typeVar(token, "size"));
  node.style.fontWeight = readToken(typeVar(token, "weight"));
  node.style.lineHeight = readToken(typeVar(token, "line"));
  node.style.letterSpacing = readToken(typeVar(token, "tracking"));
  const transform = readToken(typeVar(token, "transform"));
  if (transform !== "") {
    node.style.textTransform = transform;
  }
}

function renderTypography(): HTMLElement {
  const wrap = section("Typography");
  const list = el("div", "pg-type");
  for (const token of TYPOGRAPHY_TOKENS) {
    const row = el("div", "pg-type__row");
    const size = readToken(typeVar(token, "size"));
    const weight = readToken(typeVar(token, "weight"));
    const tracking = readToken(typeVar(token, "tracking"));
    row.append(
      el("div", "pg-type__label", `${token} · ${size} / ${weight} / ${tracking}`),
    );
    const specimen = el(
      "p",
      "pg-type__specimen",
      "Rosso Corsa — vidcull 中복 영상 검출",
    );
    applyTypeFacets(specimen, token);
    row.append(specimen);
    list.append(row);
  }
  wrap.append(list);
  return wrap;
}

function renderSpacing(): HTMLElement {
  const wrap = section("Spacing ladder");
  const list = el("div", "pg-spacing");
  for (const token of SPACING_TOKENS) {
    const value = readToken(spaceVar(token));
    const row = el("div", "pg-spacing__row");
    row.append(el("span", "pg-spacing__label", `${token} · ${value}`));
    const bar = el("div", "pg-spacing__bar");
    bar.style.width = value;
    row.append(bar);
    list.append(row);
  }
  wrap.append(list);
  return wrap;
}

function renderRadius(): HTMLElement {
  const wrap = section("Corner radius");
  const list = el("div", "pg-radius");
  for (const token of RADIUS_TOKENS) {
    const value = readToken(radiusVar(token));
    const item = el("div", "pg-radius__item");
    const box = el("div", "pg-radius__box");
    box.style.borderRadius = value;
    item.append(box, el("div", "pg-radius__label", `${token} · ${value}`));
    list.append(item);
  }
  wrap.append(list);
  return wrap;
}

function card(title: string, body: HTMLElement): HTMLElement {
  const c = el("div", "pg-card");
  c.append(el("h3", undefined, title), body);
  return c;
}

function renderComponents(): HTMLElement {
  const wrap = section("Components");
  const grid = el("div", "pg-components");

  const buttons = el("div", "pg-row");
  buttons.append(
    el("button", "btn btn--primary", "Primary CTA"),
    el("button", "btn btn--outline", "Outline"),
  );
  grid.append(card("Buttons", buttons));

  const badges = el("div", "pg-row");
  for (const label of ["EXACT", "VERY LIKELY", "POSSIBLE"]) {
    badges.append(el("span", "badge", label));
  }
  grid.append(card("Badge pill", badges));

  const inputWrap = el("div");
  const input = el("input", "input");
  input.type = "text";
  input.placeholder = "탐색할 폴더 경로";
  inputWrap.append(input);
  grid.append(card("Text input", inputWrap));

  const specRow = el("div", "pg-row");
  const spec = el("div");
  spec.append(
    el("div", "spec__value", "4K"),
    el("div", "spec__label", "resolution"),
  );
  const race = el("div");
  race.append(
    el("div", "spec__value spec__value--race", "1"),
    el("div", "spec__label", "best copy"),
  );
  specRow.append(spec, race);
  grid.append(card("Spec / race cell", specRow));

  grid.append(el("div", "livery", "Reserve Rosso Corsa for moments that matter."));

  wrap.append(grid);
  return wrap;
}

function mount(): void {
  const root = document.querySelector<HTMLElement>("#playground");
  if (!root) {
    return;
  }
  root.append(
    renderColors(),
    renderTypography(),
    renderSpacing(),
    renderRadius(),
    renderComponents(),
  );
}

mount();
