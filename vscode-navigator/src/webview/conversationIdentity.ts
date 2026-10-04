import type { HostToWebview, WebviewToHost } from "../chatProtocol";

export class ConversationIdentity {
  generation = -1;
  sessionId = "";
  constructor(private disableOldControls: () => void) {}
  receive(message: HostToWebview): boolean {
    if (typeof message.generation !== "number" || message.generation < this.generation) return false;
    if (message.generation > this.generation) {
      this.disableOldControls();
      this.generation = message.generation;
      this.sessionId = "";
    }
    if (message.type === "conversationOpened") {
      if (this.sessionId !== (message.id ?? "")) this.disableOldControls();
      this.sessionId = message.id ?? "";
    }
    return true;
  }
  bind(post: (message: WebviewToHost) => void): (message: WebviewToHost) => void {
    const generation = this.generation;
    const sessionId = this.sessionId;
    return message => {
      if (generation !== this.generation || sessionId !== this.sessionId) return;
      post({ ...message, generation, ...("sessionId" in message ? { sessionId } : {}) });
    };
  }
}
