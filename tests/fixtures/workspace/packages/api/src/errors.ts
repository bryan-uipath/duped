export interface ApiError {
  code: string;
  status: number;
  detail: string;
  retryable: boolean;
  requestId: string;
}

export interface RequestError {
  code: string;
  status: number;
  detail: string;
  retryable: boolean;
  requestId: string;
}
