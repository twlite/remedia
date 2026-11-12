/**
 * Subset of the rustdoc JSON format (`format_version` 61) the normalizer reads.
 * Field names mirror the format; see https://docs.rs/rustdoc-types for the reference definitions.
 */

export type RustId = number;

export interface RustdocCrate {
  root: RustId;
  crate_version: string | null;
  includes_private: boolean;
  index: Record<string, RustdocItem>;
  paths: Record<string, { crate_id: number; path: string[]; kind: string }>;
  external_crates: Record<string, { name: string; html_root_url: string | null }>;
  format_version: number;
}

export interface RustSpan {
  filename: string;
  begin: [number, number];
  end: [number, number];
}

export type RustAttribute = string | { other: string } | Record<string, unknown>;

export interface RustdocItem {
  id: RustId;
  crate_id: number;
  name: string | null;
  span: RustSpan | null;
  visibility: 'public' | 'default' | 'crate' | { restricted: unknown };
  docs: string | null;
  links: Record<string, RustId>;
  attrs: RustAttribute[];
  deprecation: { since: string | null; note: string | null } | null;
  inner: Record<string, unknown>;
}

export interface RustPath {
  path: string;
  id: RustId;
  args: RustGenericArgs | null;
}

export interface RustGenericArgs {
  angle_bracketed?: { args: RustGenericArg[]; constraints: RustConstraint[] };
  parenthesized?: { inputs: RustType[]; output: RustType | null };
  return_type_notation?: unknown;
}

export type RustGenericArg =
  | { lifetime: string }
  | { type: RustType }
  | { const: { expr: string; value: string | null; is_literal: boolean } }
  | 'infer';

export interface RustConstraint {
  name: string;
  args: RustGenericArgs | null;
  binding: { equality: { type: RustType } | { constant: unknown } } | { constraint: RustBound[] };
}

export type RustType =
  | { resolved_path: RustPath }
  | {
      dyn_trait: {
        traits: { trait: RustPath; generic_params: RustGenericParam[] }[];
        lifetime: string | null;
      };
    }
  | { generic: string }
  | { primitive: string }
  | {
      function_pointer: {
        sig: RustFnSig;
        generic_params: RustGenericParam[];
        header: RustFnHeader;
      };
    }
  | { tuple: RustType[] }
  | { slice: RustType }
  | { array: { type: RustType; len: string } }
  | { pat: { type: RustType } }
  | { impl_trait: RustBound[] }
  | 'infer'
  | { raw_pointer: { is_mutable: boolean; type: RustType } }
  | { borrowed_ref: { lifetime: string | null; is_mutable: boolean; type: RustType } }
  | {
      qualified_path: {
        name: string;
        args: RustGenericArgs | null;
        self_type: RustType;
        trait: RustPath | null;
      };
    };

export type RustBound =
  | { trait_bound: { trait: RustPath; generic_params: RustGenericParam[]; modifier: string } }
  | { outlives: string }
  | { use: unknown[] };

export interface RustGenericParam {
  name: string;
  kind:
    | { lifetime: { outlives: string[] } }
    | { type: { bounds: RustBound[]; default: RustType | null; is_synthetic: boolean } }
    | { const: { type: RustType; default: string | null } };
}

export interface RustGenerics {
  params: RustGenericParam[];
  where_predicates: RustWherePredicate[];
}

export type RustWherePredicate =
  | { bound_predicate: { type: RustType; bounds: RustBound[]; generic_params: RustGenericParam[] } }
  | { lifetime_predicate: { lifetime: string; outlives: string[] } }
  | { eq_predicate: { lhs: RustType; rhs: { type: RustType } | { constant: unknown } } };

export interface RustFnSig {
  inputs: [string, RustType][];
  output: RustType | null;
  is_c_variadic: boolean;
}

export interface RustFnHeader {
  is_const: boolean;
  is_unsafe: boolean;
  is_async: boolean;
  abi: string | Record<string, unknown>;
}
