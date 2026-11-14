// Record of what was sent to whom.

export interface Delivery {
  orderId: number;
  email: string;
  at: Date;
}

/** Keeps the deliveries of this process. */
export class AuditLog {
  private deliveries: Delivery[] = [];

  /** Note that the email for an order went out. */
  recordDelivery(orderId: number, email: string): void {
    this.deliveries.push({ orderId, email, at: new Date() });
  }
}
