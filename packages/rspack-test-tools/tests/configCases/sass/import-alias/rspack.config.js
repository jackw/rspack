const path = require("path");

/** @type {import("@rspack/core").Configuration} */
module.exports = {
	target: "web",
	node: false,
	module: {
		rules: [
			{
				test: /\.s[ac]ss$/i,
				use: [
					{
						loader: "sass-loader",
						options: {
							sassOptions: {
								silenceDeprecations: ["import"]
							}
						}
					}
				],
				type: "css",
				generator: {
					exportsOnly: false
				}
			}
		]
	},
	resolve: {
		alias: {
			"path-to-alias": path.resolve(__dirname, "a", `alias.scss`),
			"@scss": path.resolve(__dirname, "b", "directory-6", `_index.scss`),
			"@path-to-scss-dir": path.resolve(__dirname, "b"),
			"@/path-to-scss-dir": path.resolve(__dirname, "b")
		}
	}
};
