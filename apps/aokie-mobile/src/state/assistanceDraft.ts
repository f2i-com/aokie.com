export interface AssistanceSubmissionMarker {
  requestId: string;
  editorRevision: number;
}

export function shouldClearAssistanceDraft(
  accepted: boolean,
  submission: AssistanceSubmissionMarker,
  currentRequestId: string | null,
  currentEditorRevision: number,
): boolean {
  return accepted &&
    currentRequestId === submission.requestId &&
    currentEditorRevision === submission.editorRevision;
}
