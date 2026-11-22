import { display } from "./money.js";

export default function Amount({ cents }) {
    return <span>{display(cents)}</span>;
}
