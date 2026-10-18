import { MotionConfig } from "motion/react";
import { Hero, TopBar } from "@/sections/Top";
import { Grows, Why } from "@/sections/Story";
import { How } from "@/sections/How";
import { Footer, Get, Proof, Safety } from "@/sections/End";

export function App() {
  return (
    <MotionConfig reducedMotion="user">
      <a className="skip" href="#why">
        Skip to content
      </a>
      <TopBar />
      <main>
        <Hero />
        <Why />
        <Grows />
        <How />
        <Proof />
        <Safety />
        <Get />
      </main>
      <Footer />
    </MotionConfig>
  );
}
