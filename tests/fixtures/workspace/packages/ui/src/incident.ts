export interface Incident {
  incidentId: string;
  severity: 'error' | 'warning';
  message: string;
  occurredAt: number;
  elementId: string;
  runRef: string;
}
