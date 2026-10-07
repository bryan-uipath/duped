/** Redeclared session; has fallen one field behind core's. */
export interface SessionInfo {
  sessionId: string;
  userId: string;
  startedAt: number;
  expiresAt: number;
  scopes: string[];
}
