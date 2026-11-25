// Prices are integer cents; display labels use €.
export function subtotal(lines) {
    return lines.reduce((sum, { cents, count }) => sum + cents * count, 0);
}

export function display(cents) {
    return `€${(cents / 100).toFixed(2)}`;
}
