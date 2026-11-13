import {
  ClockIcon,
  Cog6ToothIcon,
  InboxArrowDownIcon,
  PlayIcon,
} from "@heroicons/react/24/outline";

export const navigation = [
  { name: "Automations", href: "/automations", icon: ClockIcon },
  { name: "Runs", href: "/runs", icon: PlayIcon },
  { name: "Review requests", href: "/review-requests", icon: InboxArrowDownIcon },
  { name: "Settings", href: "/settings", icon: Cog6ToothIcon },
];
