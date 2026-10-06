export interface AuthContext {
  accessToken: string;
  tenantId: string;
  organizationId: string;
  tenantName?: string;
  baseUrl?: string;
}
