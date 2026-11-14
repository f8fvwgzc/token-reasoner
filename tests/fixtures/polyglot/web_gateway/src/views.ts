// Server-side HTML views.

/** Render a view to an HTML page. */
export function render(view: string, data: unknown): string {
  const payload = JSON.stringify(data);
  return `<main data-view="${view}">${payload}</main>`;
}
