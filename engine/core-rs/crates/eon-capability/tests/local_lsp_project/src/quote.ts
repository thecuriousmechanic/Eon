import { subtotal } from "./money.js";

export interface Line {
    cents: number;
    count: number;
}

export function quote<T extends Line>(lines: readonly T[]): number {
    return subtotal(lines);
}
