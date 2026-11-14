// Email bodies.

const TEMPLATES: Record<string, string> = {
  "order-created": "Thank you, we have received order {order_id}.",
};

/** Fill a template with the given values. */
export function render(name: string, values: Record<string, string>): string {
  let text = TEMPLATES[name] ?? "";
  for (const [key, value] of Object.entries(values)) {
    text = text.replace(`{${key}}`, value);
  }
  return text;
}
