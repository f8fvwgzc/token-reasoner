// Outgoing email.
import nodemailer from "nodemailer";

export interface Mailer {
  send(to: string, subject: string, body: string): Promise<void>;
}

class BaseMailer {
  protected from = "orders@example.com";

  protected envelope(to: string, subject: string) {
    return { from: this.from, to, subject };
  }
}

/** Delivers mail through the SMTP relay. */
export class SmtpMailer extends BaseMailer implements Mailer {
  private transport = nodemailer.createTransport({ host: "smtp", port: 25 });

  async send(to: string, subject: string, body: string): Promise<void> {
    await this.transport.sendMail({ ...this.envelope(to, subject), text: body });
  }
}

/** Send one email with the default mailer. */
export async function sendEmail(to: string, subject: string, body: string): Promise<void> {
  await new SmtpMailer().send(to, subject, body);
}
