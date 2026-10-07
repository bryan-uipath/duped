export interface Session {
  sessionId: string;
  userId: string;
  startedAt: number;
  expiresAt: number;
  scopes: string[];
  device: string;
}
