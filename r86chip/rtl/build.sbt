ThisBuild / scalaVersion := "2.13.18"
ThisBuild / organization := "r86chip"
ThisBuild / version := "0.1.0"

libraryDependencies += "org.chipsalliance" %% "chisel" % "7.12.0"
addCompilerPlugin("org.chipsalliance" % "chisel-plugin" % "7.12.0" cross CrossVersion.full)
scalacOptions ++= Seq("-deprecation", "-feature", "-unchecked", "-Xfatal-warnings")
