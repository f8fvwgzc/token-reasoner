import { handleOrderCreated } from "./consumer";

jest.mock("./mailer");

describe("handleOrderCreated", () => {
  it("records the delivery of the email", async () => {
    const audit = { recordDelivery: jest.fn() };
    const tpl = { render: jest.fn() };
    const message = { content: Buffer.from(JSON.stringify({ order_id: 7, customer_id: 3 })) };

    await handleOrderCreated(message, audit, tpl);

    expect(audit.recordDelivery).toHaveBeenCalled();
  });
});
