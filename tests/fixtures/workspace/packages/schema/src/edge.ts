/** Edge as stored on disk. Every other package redeclares this shape. */
export interface SerializedEdge {
  id: string;
  source: string;
  target: string;
  sourceHandle?: string;
  targetHandle?: string;
}
