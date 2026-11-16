/**
 * The shape every label in the application has, so that §0.5's standard is structural
 * rather than remembered.
 *
 * `hint` is not optional. A field without a hint is a field whose author did not decide
 * what to tell the operator, and the one-line explanation is the cheapest documentation
 * in the product: it is in place, it is on a phone, and it cannot drift from the form
 * because it is the same object the form renders.
 */
export interface FieldCopy {
  /** 1-4 words, sentence case, plain. */
  readonly label: string;
  /** One line, always visible, <= 90 characters: what to put here and why. */
  readonly hint: string;
  /** The optional 2-5 sentence disclosure behind "More" -- real prose, not a fragment. */
  readonly more?: string;
}

/** A page's own heading and the sentence under it. */
export interface PageCopy {
  readonly title: string;
  readonly blurb?: string;
}

/** Narrows an object literal to `FieldCopy` without widening its string types. */
export function field<T extends FieldCopy>(c: T): T {
  return c;
}
