export interface Paging {
  page: number;
  pageSize: number;
  totalItems: number;
  totalPages: number;
  cursor?: string;
}
