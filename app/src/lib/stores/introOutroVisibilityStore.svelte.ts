
let shown = $state(false);

export const introOutroVisibilityStore = {
  get shown(): boolean {
    return shown;
  },
  set(next: boolean): void {
    shown = next;
  },
  toggle(): void {
    shown = !shown;
  },
};
